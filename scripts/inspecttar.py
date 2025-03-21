#!/usr/bin/env python

import tarfile
import json
from pathlib import Path
import argparse

types = {}
for k in 'REGTYPE AREGTYPE LNKTYPE SYMTYPE DIRTYPE FIFOTYPE CONTTYPE CHRTYPE BLKTYPE GNUTYPE_SPARSE'.split():
    types[getattr(tarfile, k)] = k

formats = {}
for k in 'USTAR_FORMAT GNU_FORMAT PAX_FORMAT'.split():
    formats[getattr(tarfile, k)] = k

def print_tarfile(filename, layer_index=None):
    tf = tarfile.open(filename)
    # print(dir(tf))
    # for k in dir(tf):
    #     print(k, getattr(tf, k))
    #print(formats)
    format_s = formats[tf.format]
    print(f'{filename}: format={format_s}')

    if tf.pax_headers:
        print('--- PAX ---')
        for k, v in tf.pax_headers.items():
            print(f'{k:20} {v}')

    for x in tf:
        type_s = types[x.type]
        if layer_index is None:
            print(f'size={x.size:10} mtime={x.mtime} mode={x.mode:06o} type={type_s} uid/gid={x.uid}/{x.gid} uname/gname={x.uname}/{x.gname} dev={x.devmajor},{x.devminor} {x.pax_headers} {x.name} ')
        else:
            print(f'layer={layer_index} size={x.size:10} mtime={x.mtime} mode={x.mode:06o} type={type_s} uid/gid={x.uid}/{x.gid} uname/gname={x.uname}/{x.gname} dev={x.devmajor},{x.devminor} {x.pax_headers} {x.name} ')

# expects a manifest
def main_json(index_filename, layer_index=None):
    def digest_path(digest):
        return index_filename.parent / 'blobs' / digest.replace(':', '/')

    with open(index_filename) as fh:
        index = json.load(fh)
    if len(index['manifests']) != 1: raise Exception('expecting 1 manifest')
    if index['manifests'][0]['mediaType'] != 'application/vnd.oci.image.manifest.v1+json': raise Exception('expecting manifest+v1', m['manifests'][0]['mediaType'])

    manifest_digest = index['manifests'][0]['digest']
    with open(digest_path(manifest_digest)) as fh:
        m = json.load(fh)

    if layer_index is None:
        for i, layer in enumerate(m['layers']):
            digest = layer['digest']
            print(f'-- layer {i} {digest}')
            print_tarfile(digest_path(digest), layer_index=i)

    else:
        layer = m['layers'][layer_index]
        digest = layer['digest']
        print(f'-- layer {layer_index} {digest}')
        print_tarfile(digest_path(digest), layer_index=layer_index)


def main(args):
    if args.json or args.file.suffix == '.json':
        main_json(args.file, args.layer)
    else:
        print_tarfile(args.file)

def args():
    parser = argparse.ArgumentParser()
    parser.add_argument('--json', default=False, action='store_true')
    parser.add_argument('--layer', default=None, type=int)
    parser.add_argument('file', type=Path)
    args = parser.parse_args()
    return args

main(args())
