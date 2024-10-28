package main

// v1 "github.com/google/go-containerregistry/pkg/v1"
import (
	"archive/tar"
    "fmt"
    "bytes"
    "os"
	"errors"
    "io"
    "strings"
	"path/filepath"
    "encoding/json"

	"github.com/google/go-containerregistry/pkg/name"
	"github.com/google/go-containerregistry/pkg/v1"
	"github.com/google/go-containerregistry/pkg/v1/empty"
	"github.com/google/go-containerregistry/pkg/v1/layout"
	"github.com/google/go-containerregistry/pkg/v1/remote"
	//"github.com/google/go-containerregistry/pkg/v1/mutate"

    sylabsmutate "github.com/sylabs/oci-tools/pkg/mutate"
)

const ImageRefName = "org.opencontainers.image.ref.name"

type HeaderXform func(*tar.Header) (error)

type PEImageIndexEntry struct {
    Rootfs string         `json:"rootfs"`
    Config *v1.ConfigFile `json:"config"`
}

// only fields with capital/exported are put in JSON
type PEImageIndex struct {
    Entries []PEImageIndexEntry `json:"entries"`
}

func OffsetUidGid(offset int) HeaderXform {
    return func(header *tar.Header) error {
        header.Uid += offset
        header.Gid += offset
        return nil
    }
}

func PrependPath(s string) HeaderXform {
    return func(header *tar.Header) error {
        header.Name = filepath.Join(s, header.Name)
        return nil
    }
}

func flatten(writer *tar.Writer, img v1.Image, fs ...HeaderXform) (error) {
    simg, err := sylabsmutate.Squash(img)
    if err != nil {
        return fmt.Errorf("squashing img %w", err)
    }
    layers, err := simg.Layers()
    if err != nil {
        return fmt.Errorf("retrieving image layers: %w", err)
    }
    if len(layers) != 1 {
        return fmt.Errorf("exepcted 1 layer, got %d", len(layers))
    }
    layerReader, err := layers[0].Uncompressed()
    if err != nil {
        return fmt.Errorf("getting image layer[0] reader: %w", err)
    }
    defer layerReader.Close()
    tarReader := tar.NewReader(layerReader)
    for {
        header, err := tarReader.Next()
        if errors.Is(err, io.EOF) {
            break
        }
        if err != nil {
            return fmt.Errorf("reading tar: %w", err)
        }
        header.Format = tar.FormatPAX
        if header.Uname != "" {
            fmt.Fprintf(os.Stderr, "warn: got nonempty uname %s\n", header.Uname)
        }
        if header.Gname != "" {
            fmt.Fprintf(os.Stderr, "warn: got nonempty gname %s\n", header.Gname)
        }
        for _, f := range fs {
            err := f(header)
            if err != nil {
                return fmt.Errorf("transforming header: %w", err)
            }
        }
        if err := writer.WriteHeader(header); err != nil {
            return fmt.Errorf("writing tar header: %w", err)
        }
        if header.Size > 0 {
            if _, err := io.CopyN(writer, tarReader, header.Size); err != nil {
                return fmt.Errorf("writing tar file: %w", err)
            }
        }
    }
    return nil
}

func tryShortenDigest(l int, digests []string) bool {
    acc := make(map[string]bool)
    for _, digest := range digests {
        x := digest[:l]
        if _, ok := acc[x]; ok {
            return false
        }
        acc[x] = true
    }
    return true
}

func shortenDigest(hashes map[string]v1.Hash) map[v1.Hash]string {
    ret := make(map[v1.Hash]string)
    if len(hashes) == 0 {
        return ret
    }
    digests := make([]string, 0, len(hashes))
    hashes_arr := make([]v1.Hash, 0, len(hashes))
    length := 0
    for _, hash := range hashes {
        x := hash.String()
        if !strings.HasPrefix(x, "sha256:") {
            panic(x)
        }
        x = x[len("sha256:"):]
        digests = append(digests, x)
        hashes_arr = append(hashes_arr, hash)
        length = max(length, len(x))
    }
    shortLen := 0
    for i := 1; i < length; i += 1 {
        if tryShortenDigest(i, digests) {
            shortLen = i
            break
        }
    }
    if shortLen == 0 {
        panic("should have succeeded")
    }
    fmt.Fprintf(os.Stderr, "shortened to %d chars\n", shortLen)
    for i, hash := range hashes_arr {
        ret[hash] = digests[i][:shortLen]
    }
    return ret
}

type ExportData struct {
    arg string // name passed by user
    ref name.Reference // parsed name
    refName string // ImageRefName ie org.opencontainers.image.ref.name like index.docker.io/library/gcc:13.2.0
    digest v1.Hash // image digest
    image v1.Image
    config *v1.ConfigFile
    rootfs string // shortened digest without sha256 that is the rootfs in the tar stream
}

func mainExport(args []string) (error) {
    if len(args) < 2 {
        return fmt.Errorf("expected <oci dir> <names...>")
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
    mapping, err := makeImageIndexRefName(idx) // index.docker.io/library/gcc:14.1.0 -> v1.Hash
    if err != nil {
        return fmt.Errorf("making image index map %w", err)
    }
    shortMapping := shortenDigest(mapping)    // v1.Hash -> "abcdefg"

    entries := make([]PEImageIndexEntry, 0, len(args) - 1)
    data := make([]ExportData, 0, len(args) - 1)
    seen := make(map[string]bool)

    for _, arg := range args[1:] {
        parsed, err := name.ParseReference(arg)
        if err != nil {
            return fmt.Errorf("parsing ref %w", err)
        }
        refName := parsed.Name()
        if parsed.Identifier() == "latest" {
            return fmt.Errorf("latest tag is not allowed %s: %s", arg, refName)
        }
        digest, ok := mapping[refName]
        if !ok {
            return fmt.Errorf("missing %s: %s", arg, refName)
        }
        if _, ok := seen[refName]; ok {
            return fmt.Errorf("duplicate name %s: %s", arg, refName)
        }
        seen[refName] = true
        img, err := idx.Image(digest)
        if err != nil {
            return fmt.Errorf("getting img %s %w", arg, err)
        }
        fmt.Fprintf(os.Stderr, "will write %s: %s %v\n", arg, refName, digest)
        //imgs[refName] = img
        config, err := img.ConfigFile()
        if err != nil {
            return fmt.Errorf("getting config %s %w", arg, err)
        }
        rootfs, ok := shortMapping[digest]
        if !ok {
            panic("should have gotten a short mapping")
        }
        entries = append(entries, PEImageIndexEntry {
            Rootfs: rootfs,
            Config: config,
        })
        data = append(data, ExportData {
            arg: arg,
            ref: parsed,
            refName: refName,
            digest: digest,
            image: img,
            config: config,
            rootfs: rootfs,
        })
    }

    tarWriter := tar.NewWriter(os.Stdout)
    defer tarWriter.Close()

    peidx := PEImageIndex { Entries: entries }
    peidxBuf, err := json.Marshal(&peidx)
    if err != nil {
        return fmt.Errorf("error writing index.json")
    }
    fmt.Fprintf(os.Stderr, "index.json is %s\n", peidxBuf)

    indexHeader := &tar.Header{
		Typeflag: tar.TypeReg,
		Name:     "index.json",
		Mode:     0o400,
		Size:     int64(len(peidxBuf)),
		Format:   tar.FormatPAX,
        Uid:      1000,
        Gid:      1000,
	}

    if err := tarWriter.WriteHeader(indexHeader); err != nil {
        return fmt.Errorf("writing tar index.json header: %w", err)
    }
    if _, err := io.Copy(tarWriter, bytes.NewReader(peidxBuf)); err != nil {
        return fmt.Errorf("writing tar index.json file: %w", err)
    }

    // TODO we have a bug when the different tags map to the same digest
    // it should be fine to share rootfs
    // we just need to dedup and handle that before calling shortenDigest
    // except we can't really do that because we aren't actually checking the layer identities
    // before squashing them
    for _, item := range data {
        err = flatten(tarWriter, item.image, OffsetUidGid(1000), PrependPath(item.rootfs))
        if err != nil {
            return fmt.Errorf("flattening %s %w", item.refName, err)
        }
    }
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
