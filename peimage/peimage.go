package main

import (
	"archive/tar"
	"crypto/sha256"
	"path/filepath"
    "bytes"
    "encoding/binary"
    "encoding/json"
    "errors"
    "fmt"
    "io"
    "maps"
    "os"
    "os/exec"
    "slices"
    "strings"
    "syscall"

	"github.com/google/go-containerregistry/pkg/name"
	"github.com/google/go-containerregistry/pkg/v1"
	"github.com/google/go-containerregistry/pkg/v1/empty"
	"github.com/google/go-containerregistry/pkg/v1/layout"
	"github.com/google/go-containerregistry/pkg/v1/remote"
	"github.com/google/go-containerregistry/pkg/v1/types"

    sylabsmutate "github.com/sylabs/oci-tools/pkg/mutate"
)

// I use Errorf a lot of places without a %w and get a %!w(MISSING) warning at runtime so maybe that is wrong
// but it doesn't really look like it from the docs?
// grep Errorf peimage.go | grep -v '%w'
// I guess the replacement is errors.New(fmt.Sprintf("...", ...))

const ImageRefName = "org.opencontainers.image.ref.name"
const UidGidOffset = 1000
const TwoMBAlignment = 0x20_0000  // 2MB alignment size
const IndexJsonMagic = uint64(0x1db56abd7b82da38)  // magic to be put at end of image

type HeaderXform func(*tar.Header) (error)

type PEImageId struct {
    Digest v1.Hash       `json:"digest"`
    Repository string    `json:"repository"` // library/gcc
    Registry string      `json:"registry"`   // index.docker.io
    Tag string           `json:"tag"`        // 14.1.0
}

// only fields with capital/exported are put in JSON
type PEImageIndexEntry struct {
    // [a-f0-9]+ that we have unpacked the flattened rootfs under
    Rootfs string            `json:"rootfs"`
    Config *v1.ConfigFile    `json:"config"`
    // we guarantee that descriptor.annotations["org.opencontainers.image.ref.name"] exists
    // and looks like index.docker.io/library/gcc:14.1.0
    Manifest v1.Manifest     `json:"manifest"`
    Descriptor v1.Descriptor `json:"descriptor"`
    // looks like index.docker.io/library/gcc:14.1.0
    Id PEImageId             `json:"id"` // idk this name is terrible
}

type PEImageIndex struct {
    Images []PEImageIndexEntry `json:"images"`
}

type OCIIndexEntry struct {
    img            v1.Image
    ref            name.Reference
    descriptor     v1.Descriptor
    manifest       v1.Manifest
    rootfsDigest   v1.Hash  // combined hash of each layer
    configDigest   v1.Hash
    config         *v1.ConfigFile
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
        // NOTE: symlinks do NOT get prefixed, they look broken in a tar dump
        // but resolve correctly once inside a container
        if header.Typeflag == tar.TypeLink {
            header.Linkname = filepath.Join(s, header.Linkname)
        }
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

func subsetIndex(mapping map[string]OCIIndexEntry, args []string) (map[string]OCIIndexEntry, error) {
    ret := make(map[string]OCIIndexEntry)
    for _, arg := range args {
        parsed, err := name.ParseReference(arg)
        if err != nil {
            return nil, fmt.Errorf("parsing ref %w", err)
        }
        refName := parsed.Name()
        if parsed.Identifier() == "latest" {
            return nil, fmt.Errorf("latest tag is not allowed %s: %s", arg, refName)
        }
        v, ok := mapping[refName]
        if !ok {
            return nil, fmt.Errorf("missing %s: %s", arg, refName)
        }
        if _, ok := ret[refName]; ok {
            return nil, fmt.Errorf("duplicate %s: %s", arg, refName)
        }
        ret[refName] = v
    }
    return ret, nil
}

func makeRootfsMap(mapping map[string]OCIIndexEntry) (map[v1.Hash]v1.Image) {
    ret := make(map[v1.Hash]v1.Image)
    for _, v := range mapping {
        ret[v.rootfsDigest] = v.img
    }
    return ret
}

func makePEImageIndex(selected map[string]OCIIndexEntry, rootfsPrefix map[v1.Hash]string) PEImageIndex {
    images := make([]PEImageIndexEntry, 0, len(selected))
    for _, v := range selected {
        prefix, ok := rootfsPrefix[v.rootfsDigest]
        if !ok {
            panic("should be present")
        }
        images = append(images, PEImageIndexEntry {
            Id: PEImageId {
                Digest       : v.descriptor.Digest,
                Repository   : v.ref.Context().RepositoryStr(),
                Registry     : v.ref.Context().RegistryStr(),
                Tag          : v.ref.Identifier(),
            },
            Rootfs: prefix,
            Config: v.config,
            Manifest: v.manifest,
            Descriptor: v.descriptor,
        })
    }

    return PEImageIndex { Images: images }
}

func mainExport(output io.Writer, args []string) ([]byte, error) {
    if len(args) < 2 {
        return nil, fmt.Errorf("expected <oci dir> <names...>")
    }
    srcDir := args[0]
    l, err := layout.FromPath(srcDir)
    if err != nil {
        return nil, fmt.Errorf("getting oci layout %w", err)
    }
    idx, err := l.ImageIndex()
    if err != nil {
        return nil, fmt.Errorf("getting image index %w", err)
    }
    mapping, err := loadOCIIndex(idx)
    if err != nil {
        return nil, fmt.Errorf("making image index map %w", err)
    }

    selected, err := subsetIndex(mapping, args[1:])
    if err != nil {
        return nil, fmt.Errorf("choosing images %w", err)
    }
    rootfsMap := makeRootfsMap(selected)
    rootfsShortMap := makeRootfsShortMap(rootfsMap)
    peIdx := makePEImageIndex(selected, rootfsShortMap)

    fmt.Fprintf(os.Stderr, "selected \n")
    for k, v := range selected {
        fmt.Fprintf(os.Stderr, "  %s\n", k)
        fmt.Fprintf(os.Stderr, "    descriptor %v\n", v.descriptor.Digest)
        fmt.Fprintf(os.Stderr, "      schemaVersion %v\n", v.manifest.SchemaVersion)
        fmt.Fprintf(os.Stderr, "    config   %v\n", v.configDigest)
        fmt.Fprintf(os.Stderr, "    rootfs   %v\n", v.rootfsDigest)
    }
    fmt.Fprintf(os.Stderr, "rootfs \n")
    for k, _ := range rootfsMap {
        prefix, ok := rootfsShortMap[k]
        if !ok {
            panic("should be present")
        }
        fmt.Fprintf(os.Stderr, "  %s: %v\n", prefix, k)
    }
    {
        peidxBuf, err := json.MarshalIndent(&peIdx, "", "  ")
        if err != nil {
            return nil, fmt.Errorf("error writing index.json")
        }
        fmt.Fprintf(os.Stderr, "index.json is\n%s\n", peidxBuf)
    }

    tarWriter := tar.NewWriter(output)
    defer tarWriter.Close()

    // write index.json
    peidxBuf, err := json.Marshal(&peIdx)
    if err != nil {
        return nil, fmt.Errorf("error writing index.json")
    }
    indexHeader := &tar.Header{
        Typeflag: tar.TypeReg,
        Name:     "index.json",
        Mode:     0o400,
        Size:     int64(len(peidxBuf)),
        Format:   tar.FormatPAX,
        Uid:      0,
        Gid:      0,
    }

    if err := tarWriter.WriteHeader(indexHeader); err != nil {
        return nil, fmt.Errorf("writing tar index.json header: %w", err)
    }
    if _, err := io.Copy(tarWriter, bytes.NewReader(peidxBuf)); err != nil {
        return nil, fmt.Errorf("writing tar index.json file: %w", err)
    }

    for rootfsDigest, img := range rootfsMap {
        prefix, ok := rootfsShortMap[rootfsDigest]
        if !ok {
            panic("should be present")
        }
        err = flatten(tarWriter, img, OffsetUidGid(UidGidOffset), PrependPath(prefix))
        if err != nil {
            return nil, fmt.Errorf("flattening rootfs %v %w", rootfsDigest, err)
        }
    }

    return peidxBuf, nil
}

func stashIndexJson(outfile string, data []byte) (error) {
    f, err := os.OpenFile(outfile, os.O_RDWR, 0644)
    if err != nil {
        return fmt.Errorf("opening file %s %w", outfile, err)
    }
    defer f.Close()

    info, err := f.Stat()
    if err != nil {
        return fmt.Errorf("stating file %s %w", outfile, err)
    }
    oldSize := int(info.Size())
    // 8 for magic, 4 for data size
    sizeNeeded := len(data) + 8 + 4
    newSize := roundUpTo(oldSize + sizeNeeded, TwoMBAlignment)
    if err = f.Truncate(int64(newSize)); err != nil {
        return fmt.Errorf("truncating file %s %w", outfile, err)
    }
    // 2 is from end
    if _, err = f.Seek(int64(-sizeNeeded), 2); err != nil {
        return fmt.Errorf("seeking file %s to %d %w", outfile, sizeNeeded, err)
    }
    if _, err = f.Write(data); err != nil {
        return fmt.Errorf("writing file data %s %w", outfile, err)
    }
    u32Size := uint32(len(data))
    if int(u32Size) != len(data) {
        return fmt.Errorf("data way too big")
    }
    if err = binary.Write(f, binary.LittleEndian, u32Size); err != nil {
        return fmt.Errorf("writing file size %s", outfile, err)
    }
    if err = binary.Write(f, binary.LittleEndian, IndexJsonMagic); err != nil {
        return fmt.Errorf("writing magic%s", outfile, err)
    }
    return nil
}

func exportImageSqfs(outfile string, args []string) (error) {
    cmd := exec.Command("sqfstar", "-comp", "zstd", "-force", outfile)
    stdin, err := cmd.StdinPipe()
    if err != nil {
        return fmt.Errorf("error getting stdin %w", err)
    }
    if err = cmd.Start(); err != nil {
        return fmt.Errorf("error starting sqfstar %w", err)
    }
    idxBuf, err := mainExport(stdin, args)
    if err != nil {
        return fmt.Errorf("error exporting %w", err)
    }
    if err = cmd.Wait(); err != nil {
        return fmt.Errorf("error waiting for sqfstar %w", err)
    }
    if err = stashIndexJson(outfile, idxBuf); err != nil {
        return fmt.Errorf("error writing index.json %w", err)
    }
    return nil
}

func exportImageErofs(outfile string, args []string) (error) {
    f, err := os.CreateTemp("", "fifo")
    if err != nil {
        return fmt.Errorf("tempfile %w", err)
    }
    f.Close()
    fifoName := f.Name()
    os.Remove(fifoName)
    if err = syscall.Mkfifo(fifoName, 0600); err != nil {
        return fmt.Errorf("mkfifo %w", err)
    }
    defer os.Remove(fifoName)

    fifo, err := os.OpenFile(fifoName, os.O_RDWR, 0600)
    if err != nil {
        return fmt.Errorf("fifo open %w", err)
    }

    cmd := exec.Command("mkfs.erofs", "--tar=f", outfile, fifoName)
    if err = cmd.Start(); err != nil {
        return fmt.Errorf("error starting mkfs.erofs %w", err)
    }
    idxBuf, err := mainExport(fifo, args)
    if err != nil {
        return fmt.Errorf("error exporting %w", err)
    }
    fifo.Close()
    if err = cmd.Wait(); err != nil {
        return fmt.Errorf("error waiting for mkfs.erofs %w", err)
    }
    if err = stashIndexJson(outfile, idxBuf); err != nil {
        return fmt.Errorf("error writing index.json %w", err)
    }
    return nil
}

func mainImage(args []string) (error) {
    if len(args) < 3 {
        return fmt.Errorf("expected <image.sqfs|image.erofs> <oci-dir> <names...>")
    }
    image := args[0]
    isSqfs := strings.HasSuffix(image, ".sqfs")
    isErofs := strings.HasSuffix(image, ".erofs")
    if !isSqfs && !isErofs {
        return fmt.Errorf("expected image to end with .sqfs or .erofs")
    }
    format := "sqfs"
    if isErofs {
        format = "erofs"
    }

    switch format {
    case "sqfs":
        return exportImageSqfs(image, args[1:])

    case "erofs":
        return exportImageErofs(image, args[1:])
    default:
        return fmt.Errorf("got unexpected format %s", format)
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
    mapping, err := loadOCIIndex(idx)
    if err != nil {
        return fmt.Errorf("making image index map %w", err)
    }
    seen := make(map[string]v1.Hash)
    for k, v := range mapping {
        seen[k] = v.descriptor.Digest
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
        if digest, ok := seen[parsed.Name()]; ok {
            fmt.Printf("already have %s: %s = %v, skipping\n", ref, parsed.Name(), digest)
            continue
        }
        if parsed.Identifier() == "latest" {
            return fmt.Errorf("latest tag is not allowed %s: %s", ref, parsed.Name())
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
        seen[parsed.Name()] = digest
    }
    return nil
}

func mainList(args []string) (error) {
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
    mapping, err := loadOCIIndex(idx)
    if err != nil {
        return fmt.Errorf("loading oci index %w", err)
    }
    fmt.Printf("oci dir %s\n", srcDir)
    for k, v := range mapping {
        fmt.Printf("%s\n", k)
        fmt.Printf("  manifest %v\n", v.manifest.MediaType)
        fmt.Printf("           %v\n", v.descriptor.Digest)
        fmt.Printf("  config   %v\n", v.configDigest)
        fmt.Printf("  rootfs   %v\n", v.rootfsDigest)
        layers, err := v.img.Layers()
        if err != nil {
            return fmt.Errorf("getting layers %s", k)
        }
        for i, layer := range layers {
            digest, err := layer.Digest()
            if err != nil {
                return fmt.Errorf("getting layer digest %s %d", k, i)
            }
            m, err := layer.MediaType()
            if err != nil {
                return fmt.Errorf("getting media type %s %d", k, i)
            }
            fmt.Printf("  %3d      %v\n", i, m)
            fmt.Printf("           %v\n", digest)
        }
    }
    return nil
}

func mainParse(args []string) (error) {
    for _, ref := range args {
        parsed, err := name.ParseReference(ref)
        if err != nil {
            return fmt.Errorf("error parsing %w", err)
        }
        fmt.Printf("%s\tName=%s\tContext=%s\tIdentifier=%s\tContext.RepositorStr=%s\tContext.RegistryStr=%s\n",
                    ref, parsed.Name(), parsed.Context(), parsed.Identifier(),
                    parsed.Context().RepositoryStr(), parsed.Context().RegistryStr(),
                )
    }
    return nil
}

func main() {
    if len(os.Args) == 1 {
        fmt.Fprintf(os.Stderr, "expected <pull|export|list|parse|image>\n");
        fmt.Fprintf(os.Stderr, "  pull <oci-dir> <names...>\n");
        fmt.Fprintf(os.Stderr, "  export <oci-dir> <names...>; writes to stdout\n");
        fmt.Fprintf(os.Stderr, "  list <oci-dir>\n");
        fmt.Fprintf(os.Stderr, "  parse <names...>\n");
        fmt.Fprintf(os.Stderr, "  image <image.sqfs|image.erofs> <oci-dir> <names...>\n");
        os.Exit(1)
    }
    err := error(nil)
    switch cmd := os.Args[1]; cmd {
    case "export":
        _, err = mainExport(os.Stdout, os.Args[2:])
    case "pull":
        err = mainPull(os.Args[2:])
    case "list":
        err = mainList(os.Args[2:])
    case "parse":
        err = mainParse(os.Args[2:])
    case "image":
        err = mainImage(os.Args[2:])
    default:
        err = fmt.Errorf("command export|pull|list|parse")
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

func layersDigest(img v1.Image) (*v1.Hash, error) {
    layers, err := img.Layers()
    if err != nil {
        return nil, fmt.Errorf("getting layers %w", err)
    }
    h := sha256.New()
    for _, layer := range layers {
        d, err := layer.Digest()
        if err != nil {
            return nil, fmt.Errorf("getting layer digest %w", err)
        }
        // https://github.com/opencontainers/image-spec/blob/main/media-types.md
        // reports application/vnd.docker.image.rootfs.diff.tar.gzip and
        //         application/vnd.oci.image.layer.v1.tar+gzip
        // as being fully compatible
        // m, err := layer.MediaType()
        // if err != nil {
        //     return nil, fmt.Errorf("getting layer media type %w", err)
        // }
        // h.Write([]byte(m))
        h.Write([]byte(d.String()))
    }
    digest := h.Sum(nil)
    s := fmt.Sprintf("sha256:%x", digest)
    v1h, err := v1.NewHash(s)
    if err != nil {
        return nil, fmt.Errorf("bad hash %w", err)
    }
    ret := new(v1.Hash)
    *ret = v1h
    return ret, nil
}

func loadOCIIndex(idx v1.ImageIndex) (map[string]OCIIndexEntry, error) {
    idxManifest, err := idx.IndexManifest()
    if err != nil {
        return nil, fmt.Errorf("getting indexManifest %w", err)
    }
    seen := make(map[v1.Hash]bool)
    ret := make(map[string]OCIIndexEntry)

    // manifest is type v1.Descriptor (the naming is crazy!)
    for _, descriptor := range idxManifest.Manifests {
        if descriptor.MediaType != types.OCIManifestSchema1 &&
           descriptor.MediaType != types.DockerManifestSchema2 {
            fmt.Fprintf(os.Stderr, "skipping manifest %v because is mediatype %v\n", descriptor.Digest, descriptor.MediaType)
            continue
        }
        imageRefName, ok := descriptor.Annotations[ImageRefName]
        if !ok {
            fmt.Fprintf(os.Stderr, "skipping manifest %v because missing annotation %s\n", descriptor.Digest, ImageRefName)
            continue
        }
        ref, err := name.ParseReference(imageRefName)
        if err != nil {
            return nil, fmt.Errorf("parsing ref %s %v", imageRefName, err)
        }
        if otherDigest, ok := ret[imageRefName]; ok {
            return nil, fmt.Errorf("Duplicate ref.name for %s %v and %v", imageRefName, otherDigest, descriptor.Digest)
        }
        if _, ok := seen[descriptor.Digest]; ok {
            return nil, fmt.Errorf("Duplicate image stored under two ref.name %s %v", imageRefName, descriptor.Digest)
        }
        img, err := idx.Image(descriptor.Digest)
        if err != nil {
            return nil, fmt.Errorf("getting img %s %w", descriptor.Digest, err)
        }
        configDigest, err := img.ConfigName()
        if err != nil {
            return nil, fmt.Errorf("getting config digest %s %w", descriptor.Digest, err)
        }
        config, err := img.ConfigFile()
        if err != nil {
            return nil, fmt.Errorf("getting config %s %w", descriptor.Digest, err)
        }
        manifest, err := img.Manifest()
        if err != nil {
            return nil, fmt.Errorf("getting manifest %s %w", descriptor.Digest, err)
        }
        rootfsDigest, err := layersDigest(img)
        if err != nil {
            return nil, fmt.Errorf("computing layers digest %s %w", descriptor.Digest, err)
        }

        seen[descriptor.Digest] = true
        ret[imageRefName] = OCIIndexEntry {
            img: img,
            ref: ref,
            descriptor: descriptor,
            manifest: *manifest,
            rootfsDigest: *rootfsDigest,
            configDigest: configDigest,
            config: config,
        }
    }
    return ret, nil
}

func tryShortenDigest(digests []string, l int) bool {
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

// we take in a map so that we are guaranteed the hashes are unique
func makeRootfsShortMap(hashesM map[v1.Hash]v1.Image) map[v1.Hash]string {
    if len(hashesM) == 0 {
        panic("shouldn't be empty")
    }
    hashes := slices.Collect(maps.Keys(hashesM))
    digests := make([]string, 0, len(hashes))
    length := 0
    for _, hash := range hashes {
        x := hash.String()
        if !strings.HasPrefix(x, "sha256:") {
            panic(x)
        }
        x = x[len("sha256:"):]
        digests = append(digests, x)
        length = max(length, len(x))
    }
    shortLen := 0
    for i := 1; i < length; i += 1 {
        if tryShortenDigest(digests, i) {
            shortLen = i
            break
        }
    }
    if shortLen == 0 {
        panic("should have succeeded")
    }
    fmt.Fprintf(os.Stderr, "shortened to %d chars\n", shortLen)
    ret := make(map[v1.Hash]string)
    for i, hash := range hashes {
        ret[hash] = digests[i][:shortLen]
    }
    return ret
}

func openFile(s string) (*os.File, error) {
	if s == "-" {
		return os.Stdout, nil
	}
	return os.Create(s)
}

func roundUpTo(x, N int) int {
    return ((x + (N - 1)) / N) * N
}
