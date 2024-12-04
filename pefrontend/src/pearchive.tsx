enum ArchiveFormat1Tag {
    File = 1,
    Dir = 2,
    Pop = 3,
}

function stripLeadingJunk(x: string): string {
    return
}

export function makeHiearachy(files: {path: string, data: string|ArrayBuffer}[]) {
    let ret = new Map();
    for (let file of files) {
        let parts = file.path.split('/');
        if (parts.length === 0) {
            continue;
        }
        let cur = ret;
        for (let part of parts.slice(0, -1)) {
            // we just skip this junk
            if (part === '' || part === '.' || part === '..') {
                continue;
            }
            if (cur[part] === undefined) {
                cur.set(part, new Map());
            } else {
                cur = cur.get(part);
            }
        }
        let name = parts[parts.length - 1];
        if (name === '' || name === '.' || name === '..') {
            throw new Error('bad file name');
        }
        cur.set(name, file);
    }
    return ret;
}

// kinda gross, not sure
function* encodeHierarchy(cur) {
    for (let [name, v] of cur.entries()) {
        if (v instanceof Map) {
            yield ArchiveFormat1Tag.Dir;
            yield* Uint8Array.from(name, x=>x.charCodeAt(0));
            yield 0; // null term
            yield* encodeHierarchy(v);
            yield ArchiveFormat1Tag.Pop;
        } else {
            yield ArchiveFormat1Tag.File;
            yield* Uint8Array.from(name, x=>x.charCodeAt(0));
            yield 0;
            if (typeof v.data === 'string') {
                yield* Uint8Array.from(name, x=>x.charCodeAt(0));
            } else {
                yield* v.data;
            }
        }
    }
}

// encodes files to pearchivev1
export function packArchiveV1(files: {path: string, data: string|ArrayBuffer}[]): Uint8Array {
    console.time('packArchiveV1');
    let hierachy = makeHiearachy(files);
    let ret = new Uint8Array(encodeHierarchy(hierachy));
    console.timeEnd('packArchiveV1');
    return ret;
}

function mergeUint8Arrays(...arrays) {
  const totalSize = arrays.reduce((acc, e) => acc + e.length, 0);
  const merged = new Uint8Array(totalSize);

  arrays.forEach((array, i, arrays) => {
    const offset = arrays.slice(0, i).reduce((acc, e) => acc + e.length, 0);
    merged.set(array, offset);
  });

  return merged;
}

export function combineRequestAndArchive(req, archive: Uint8Array): Uint8Array {
    let reqs = JSON.stringify(req);
    let reqbuf = Uint8Array.from(reqs, x=>x.charCodeAt(0));
    let len = reqbuf.length;
    let lenbuf = new ArrayBuffer(4);
    //                                     LE
    new DataView(lenbuf).setUint32(0, len, true);

    return mergeUint8Arrays(new Uint8Array(lenbuf), reqbuf, archive);
}
