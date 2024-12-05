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
function encodeHierarchy(root: Map<string, any>): Blob {
    let lenbuf = new ArrayBuffer(4);
    let lenbufview = new DataView(lenbuf);
    // new Blob([1]) == [49] because ord('1') == 49 (it calls toString()!)
    let tagDir = new Uint8Array([ArchiveFormat1Tag.Dir]);
    let tagFile = new Uint8Array([ArchiveFormat1Tag.File]);
    let tagPop = new Uint8Array([ArchiveFormat1Tag.Pop]);
    let nullByte = new Uint8Array([0]);
    let te = new TextEncoder();

    function* recur(cur) {
        for (let [name, v] of cur.entries()) {
            if (v instanceof Map) {
                console.log('dir ' + name);
                yield tagDir;
                yield te.encode(name);
                yield nullByte; // null term
                yield* recur(v);
                yield tagPop;
            } else {
                console.log('file ' + name);
                console.log(typeof ArchiveFormat1Tag.File);
                yield tagFile;
                yield te.encode(name);
                yield nullByte; // null term
                if (typeof v.data === 'string') {
                    let data = te.encode(v.data);
                    console.log('string data length', data.byteLength);
                    lenbufview.setUint32(0, data.byteLength, true);
                    console.log(new Uint8Array(lenbuf));
                    yield lenbuf.slice();
                    yield data;
                } else {
                    lenbufview.setUint32(0, v.data.byteLength, true);
                    yield lenbuf.slice();
                    yield v.data;
                }
            }
        }
    }

    return new Blob(recur(root));
}

// encodes files to pearchivev1
export function packArchiveV1(files: {path: string, data: string|ArrayBuffer}[]): Blob {
    console.time('packArchiveV1');
    let hierachy = makeHiearachy(files);
    let ret = encodeHierarchy(hierachy);
    console.timeEnd('packArchiveV1');
    return ret;
}

export function combineRequestAndArchive(req, archive: Blob): Blob {
    let te = new TextEncoder();
    let reqbuf = te.encode(JSON.stringify(req));
    let lenbuf = new ArrayBuffer(4);
    console.log(reqbuf);
    //                                     LE
    new DataView(lenbuf).setUint32(0, reqbuf.byteLength, true);

    return new Blob([lenbuf, reqbuf, archive]);
}

// we use DataView as a standin for a ArrayBuffer slice
export function splitResponseAndArchive(buf: ArrayBuffer): [any, DataView] {
    let lenview = new DataView(buf);
    let responseLen = lenview.getUint32(0, true);
    let responseView = new DataView(buf, 4, responseLen);
    console.log(responseView);
    let responseString = new TextDecoder().decode(responseView);
    let responseJson = JSON.parse(responseString);
    let archiveSlice = new DataView(buf, 4 + responseLen);
    return [responseJson, archiveSlice];
}
