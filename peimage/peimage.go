package main

// v1 "github.com/google/go-containerregistry/pkg/v1"
import (
    "fmt"
    "os"
    "io"

	"github.com/google/go-containerregistry/pkg/name"
	"github.com/google/go-containerregistry/pkg/v1"
	"github.com/google/go-containerregistry/pkg/v1/empty"
	"github.com/google/go-containerregistry/pkg/v1/layout"
	"github.com/google/go-containerregistry/pkg/v1/remote"

    "github.com/sylabs/oci-tools/pkg/mutate"
)

const ImageRefName = "org.opencontainers.image.ref.name"

func squash(src, dst string) (error) {
    dstFile, err := openFile(dst)
    if err != nil {
        return fmt.Errorf("opening dst file %s: %w", dst, err)
    }

    l, err := layout.ImageIndexFromPath(src)
    if err != nil {
        return fmt.Errorf("loading %s as OCI layout: %w", src, err)
    }

    m, err := l.IndexManifest()
    if err != nil {
        return fmt.Errorf("reading index manifest %s %w", src, err)
    }
    if len(m.Manifests) != 1 {
        return fmt.Errorf("layout contains %d entries", len(m.Manifests))
    }

    desc := m.Manifests[0]
    if !desc.MediaType.IsImage() {
        return fmt.Errorf("not an image %s %w", src, err)
    }

    img, err := l.Image(desc.Digest)
    if err != nil {
        return fmt.Errorf("reading image %s %w", src, err)
    }

    simg, err := mutate.Squash(img)
    if err != nil {
        return fmt.Errorf("squashing image %s %w", src, err)
    }

	layers, err := simg.Layers()
	if err != nil {
		return fmt.Errorf("retrieving image layers: %w", err)
	}

    if len(layers) != 1 {
		return fmt.Errorf("exepcted 1 layer, got %d", len(layers))
    }

    reader, err := layers[0].Uncompressed()
	if err != nil {
		return fmt.Errorf("getting image layer[0] reader: %w", err)
	}

    _, err = io.Copy(dstFile, reader)
    if err != nil {
		return fmt.Errorf("writing output: %w", err)
    }
    return nil
}

func mainExport(args []string) (error) {
    return nil
}

func mainPull(args []string) (error) {
    if len(args) < 2 {
        return fmt.Errorf("expected <oci dir> <names...>")
    }
    srcDir := args[0]
    l, err := getOrCreateOciLayout(srcDir)
    if err != nil {
        return fmt.Errorf("getting oci layout %w", err)
    }
    idx, err := l.ImageIndex()
    if err != nil {
        return fmt.Errorf("getting image index %w", err)
    }
    mapping, err := makeImageIndexRefName(idx)
    if err != nil {
        return fmt.Errorf("making image index map %w", err)
    }
    platform, err := v1.ParsePlatform("linux/amd64")  // is the default but good to be specific
    if err != nil {
        return fmt.Errorf("parsing platform %w", err)
    }
    remoteOpts := []remote.Option{}
    remoteOpts = append(remoteOpts, remote.WithPlatform(*platform))

    for _, ref := range args[1:] {
        parsed, err := name.ParseReference(ref)
        if err != nil {
            return fmt.Errorf("parsing ref %w", err)
        }
        if digest, ok := mapping[parsed.Name()]; ok {
            fmt.Printf("already have %s: %s = %v, skipping\n", ref, parsed.Name(), digest)
            continue
        }
        // based on go-containerregistry/cmd/crane/cmd/pull.go
        opts := []layout.Option{}
        opts = append(opts, layout.WithAnnotations(map[string]string{
            ImageRefName: parsed.Name(),
        }))
        rmt, err := remote.Get(parsed, remoteOpts...)
        if err != nil {
            return fmt.Errorf("getting remote %w", err)
        }
        fmt.Printf("fetching %s: %s\n", ref, parsed.Name())
        img, err := rmt.Image()
        if err != nil {
            return fmt.Errorf("getting image %w", err)
        }
        if err = l.AppendImage(img, opts...); err != nil {
            return err
        }
        digest, err := img.Digest()
        if err != nil {
            return err
        }
        fmt.Printf("pulled %s: %s = %v\n", ref, parsed.Name(), digest)
        mapping[parsed.Name()] = digest
    }
    return nil
}

func mainInfo(args []string) (error) {
    if len(args) != 1 {
        return fmt.Errorf("expected <oci dir>")
    }
    srcDir := args[0]
    l, err := layout.FromPath(srcDir)
    if err != nil {
        return fmt.Errorf("getting oci layout %w", err)
    }
    idx, err := l.ImageIndex()
    if err != nil {
        return fmt.Errorf("getting image index %w", err)
    }
    mapping, err := makeImageIndexRefName(idx)
    if err != nil {
        return fmt.Errorf("making image index map %w", err)
    }
    fmt.Printf("oci dir %s\n", srcDir)
    for k, v := range mapping {
        fmt.Printf("  %s = %s\n", k, v)
    }
    return nil
}

func mainParse(args []string) (error) {
    for _, ref := range args {
        parsed, err := name.ParseReference(ref)
        if err != nil {
            return fmt.Errorf("error parsing %w", err)
        }
        fmt.Printf("%s\tName=%s\tContext=%s\tIdentifier=%s\n", ref, parsed.Name(), parsed.Context(), parsed.Identifier())
    }
    return nil
}

func main() {
    if len(os.Args) == 1 {
        fmt.Fprintf(os.Stderr, "expected <pull|export|info|parse> <names ...> %w\n");
        os.Exit(1)
    }
    err := error(nil)
    switch cmd := os.Args[1]; cmd {
    case "export":
        err = mainExport(os.Args[2:])
    case "pull":
        err = mainPull(os.Args[2:])
    case "info":
        err = mainInfo(os.Args[2:])
    case "parse":
        err = mainParse(os.Args[2:])
    }

    if err != nil {
        fmt.Fprintf(os.Stderr, "fail %s\n", err);
        os.Exit(1)
    }
}

func getOrCreateOciLayout(dir string) (*layout.Path, error) {
    l, err := layout.FromPath(dir)
    if err != nil {
        l, err = layout.Write(dir, empty.Index)
        if err != nil {
            return nil, err
        }
    }
    ret := new(layout.Path)
    *ret = l
    return ret, nil
}

func makeImageIndexRefName(idx v1.ImageIndex) (map[string]v1.Hash, error) {
    idxManifest, err := idx.IndexManifest()
    if err != nil {
        return nil, fmt.Errorf("getting indexManifest %w", err)
    }
    ret := make(map[string]v1.Hash)
    for _, manifest := range idxManifest.Manifests {
        if imageRefName, ok := manifest.Annotations[ImageRefName]; ok {
            if otherDigest, ok := ret[imageRefName]; ok && manifest.Digest != otherDigest {
                return nil, fmt.Errorf("Got two different digests for same %s %v != %v", ImageRefName, otherDigest, manifest.Digest)
            }
            ret[imageRefName] = manifest.Digest
        }
    }
    return ret, nil
}

func openFile(s string) (*os.File, error) {
	if s == "-" {
		return os.Stdout, nil
	}
	return os.Create(s)
}
