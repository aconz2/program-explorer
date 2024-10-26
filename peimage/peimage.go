package main

// v1 "github.com/google/go-containerregistry/pkg/v1"
import (
    "fmt"
    "os"
    "io"


	"github.com/google/go-containerregistry/pkg/v1/layout"
    mutate "github.com/sylabs/oci-tools/pkg/mutate"
)

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
        return fmt.Errorf("layout contains %d entries, consider --index", len(m.Manifests))
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

func main() {
    if len(os.Args) != 3 {
        fmt.Fprintf(os.Stderr, "expect <src> <dst> %w\n", os.Args);
        os.Exit(1)
    }
    src := os.Args[1]
    dst := os.Args[2]

    err := squash(src, dst)
    if err != nil {
        fmt.Fprintf(os.Stderr, "fail %s\n", err);
        os.Exit(1)
    }
}

func openFile(s string) (*os.File, error) {
	if s == "-" {
		return os.Stdout, nil
	}
	return os.Create(s)
}
