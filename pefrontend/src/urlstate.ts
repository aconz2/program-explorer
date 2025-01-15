import {bufFromBase64} from './util';

export type UrlHashState = {
    // just for dev
    expand: {
        help: boolean,
        more: boolean,
    },
    cmd: string | null,
    stdin: string | null,
    env: string | null,
    image: string | null,
    files: {path: string, data: string}[] | null,
}

type UrlHashStateSettings = {
    cmd?: string | null,
    stdin?: string | null,
    env?: string | null,
    image?: string | null,
    files?: ({p: string, s: string} | {p: string, b: string})[],
}

export function loadUrlHashState(): UrlHashState { return parseUrlHashState(window.location.hash); }
export function encodeUrlHashState(x: {
    cmd: string,
    stdin: string,
    env: string,
    image: string,
    files: ({p: string, s: string} | {p: string, b: string})[]
}): string {
    return window.btoa(JSON.stringify(x));
}
// chrome doesn't support https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Uint8Array/fromBase64 yet
// so we can't do Uint8Array.fromBase64 yet; punt and only handle strings for now
function tryBase64Decode(x: string | null | undefined): string | null {
    if (x == null) return null;
    try {
        return window.atob(x);
    } catch {
        return null;
    }
}
function checkString(x): string | null { return typeof x === 'string' ? x : null; }
function checkStringArray(x): string[] | null {
    if (!Array.isArray(x)) return null;
    if (!x.every((y) => typeof y === 'string')) return null;
    return x;
}
function checkFiles(x): ({p: string, s: string} | {p: string, b: string})[] | null {
    if (!Array.isArray(x)) return null;
    let ret = [];
    for (let y of x) {
        let path = y.p;
        if (path == null) return null;
        let data;
        if (y.s != null && typeof y.s === 'string') {
            data = y.s;
        } else if (y.b != null && typeof y.b === 'string') {
            data = bufFromBase64(y.b);
            if (data == null) {
                console.error('got null data from atob?');
                return null;
            }
        } else {
            console.error('unhandled case');
            return null;
        }
        ret.push({path, data});
    }
    return ret;
}

function decodeBase64Json(s): object {
    try {
        return JSON.parse(window.atob(s));
    } catch (e) {
        console.error('error decoding json', e);
        return {};
    }
}

function decodeSettings(s: string): UrlHashStateSettings {
    return decodeBase64Json(s);
}

function parseUrlHashState(s): UrlHashState {
    let ret = {
        expand: { help: false, more: false, },
        cmd: null,
        stdin: null,
        env: null,
        image: null,
        files: null,
    };
    let parts = s.substring(1).split('&');
    for (let part of parts) {
        let [a, b] = part.split('=');
        if      (a === 'help' && b === 'x') { ret.expand.help = true; }
        else if (a === 'more' && b === 'x') { ret.expand.more = true; }
        else if (a === 's') {
            let settings = decodeSettings(b);
            ret.cmd = checkString(settings.cmd);
            ret.stdin = checkString(settings.stdin);
            ret.image = checkString(settings.image);
            ret.env = checkString(settings.env);
            ret.files = checkFiles(settings.files);
        }
    }
    return ret;
}

