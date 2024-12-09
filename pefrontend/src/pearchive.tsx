const MAX_NAME_LEN = 255; // tmpfs max name length

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
            let x = cur.get(part);
            if (x === undefined) {
                x = new Map();
                cur.set(part, x);
            }
            cur = x;
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
    // so we have to make a u8 array for each tag (and null)
    let tagDir = new Uint8Array([ArchiveFormat1Tag.Dir]);
    let tagFile = new Uint8Array([ArchiveFormat1Tag.File]);
    let tagPop = new Uint8Array([ArchiveFormat1Tag.Pop]);
    let nullByte = new Uint8Array([0]);
    let te = new TextEncoder();

    function* recur(cur) {
        for (let [name, v] of cur.entries()) {
            if (v instanceof Map) {
                yield tagDir;
                yield te.encode(name);
                yield nullByte; // null term
                yield* recur(v);
                yield tagPop;
            } else {
                yield tagFile;
                yield te.encode(name);
                yield nullByte; // null term
                if (typeof v.data === 'string') {
                    let data = te.encode(v.data);
                    lenbufview.setUint32(0, data.byteLength, /* LE */ true);
                    yield lenbuf.slice();
                    yield data;
                } else {
                    lenbufview.setUint32(0, v.data.byteLength, /* LE */ true);
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

function findZeroByte(buf: DataView, start: number): number {
    for (let i = start; i < Math.min(start + MAX_NAME_LEN, buf.byteLength); i++) {
        if (buf.getUint8(i) === 0) return i;
    }
    return -1;
}


// tries to decode as utf-8, if fails, returns as arraybuffer and you can retry with another encoding
// okay we don't actually respect the byteLength of a DataView since we read the length from the archive and slice
// a new one from the underlying buffer. But really we just need it for the offset
export function unpackArchiveV1(data: ArrayBuffer|Uint8Array|DataView): {path: string, data: string|ArrayBuffer}[] {
    console.time('unpackArchiveV1');
    let i = (data instanceof DataView) ? data.byteOffset : 0;
    // note we recreate a view if given a view and always just work with the offset it gave
    let view = (data instanceof ArrayBuffer) ? new DataView(data) : new DataView(data.buffer);
    let buffer = (data instanceof ArrayBuffer) ? data : data.buffer;
    const n = view.byteLength;

    let lenbuf = new ArrayBuffer(4);
    let lenbufview = new DataView(lenbuf);
    let te = new TextDecoder('utf-8', {fatal: true});
    let acc = [];
    let pathBuf = [];

    // decode as utf-8 or copy the slice as a DataView (so that we can free the original blob eventually)
    function extractFile(view: DataView): string | ArrayBuffer {
        try {
            return te.decode(view);
        } catch {
            return view.buffer.slice(view.byteOffset, view.byteOffset + view.byteLength);
        }
    }

    while (i < n) {
        let tag = view.getUint8(i);
        i++;
        switch (tag) {
            case ArchiveFormat1Tag.File: {
                let zbi = findZeroByte(view, i);
                if (zbi === -1) { throw new Error("didnt get null byte"); } // TODO
                let nameLen = zbi - i;
                let name = te.decode(new DataView(buffer, i, nameLen));
                pathBuf.push(name);
                let path = pathBuf.join('/');
                pathBuf.pop();
                let len = view.getUint32(zbi+1, /* LE */ true);
                i = zbi + 1 + 4;
                let fileView = new DataView(buffer, i, len); // this is where we don't respect a DataView.byteLength
                let data = extractFile(fileView);
                i += len;
                acc.push({path, data});
                break;
            }
            case ArchiveFormat1Tag.Dir: {
                let zbi = findZeroByte(view, i);
                if (zbi === -1) { throw new Error("didnt get null byte"); } // TODO
                let nameLen = zbi - i;
                let name = te.decode(new DataView(buffer, i, nameLen));
                pathBuf.push(name);
                i = zbi + 1;
                break;
            }
            case ArchiveFormat1Tag.Pop:
                pathBuf.pop();
                break;
            default:
                return acc;
        }
    }

    console.log(acc);
    console.timeEnd('unpackArchiveV1');
    return acc;
}

// <u32: json len> <json> <archive>
export function combineRequestAndArchive(req, archive: Blob): Blob {
    let te = new TextEncoder();
    let reqbuf = te.encode(JSON.stringify(req));
    let lenbuf = new ArrayBuffer(4);
    new DataView(lenbuf).setUint32(0, reqbuf.byteLength, /* LE */ true);

    return new Blob([lenbuf, reqbuf, archive]);
}

// we use DataView as a standin for a ArrayBuffer slice
export function splitResponseAndArchive(buf: ArrayBuffer): [any, DataView] {
    let lenview = new DataView(buf);
    let responseLen = lenview.getUint32(0, true);
    let responseView = new DataView(buf, 4, responseLen);
    let responseString = new TextDecoder().decode(responseView);
    let responseJson = JSON.parse(responseString);
    let archiveSlice = new DataView(buf, 4 + responseLen);
    return [responseJson, archiveSlice];
}
