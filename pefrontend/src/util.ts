
export function bufToHex(data: ArrayBuffer, length: number): string {
    let n = Math.min(data.byteLength, length);
    let acc = '';
    let hexDigit = (i) => '0123456789abcdef'[i];
    if (data instanceof ArrayBuffer) {
        let buf = new Uint8Array(data);
        for (let i = 0; i < n; i++) {
            let b = buf[i];
            acc +=  hexDigit((b >> 4) & 0xf) + hexDigit(b & 0xf);
        }
        return acc;
    }
    throw new Error('bad type');
}

export function debounce(f, wait) {
  let timeoutId = null;
  return (...args) => {
    window.clearTimeout(timeoutId);
    timeoutId = window.setTimeout(() => {
      f(...args);
    }, wait);
  };
}

export function parseEnvText(s: string): string[] {
    let ret = [];
    for (let line of s.split('\n')) {
        if (line.startsWith('#')) {
            continue;
        }
        // TODO do some validation like VAR=blah
        ret.push(line);
    }
    return ret;
}

function bufToBase64Native(x: ArrayBuffer): string {
    // @ts-ignore:next-line
    return (new Uint8Array(x)).toBase64();
}
function bufToBase64Slow(x: ArrayBuffer): string {
    let ret = '';
    const bytes = new Uint8Array(x);
    const len = bytes.byteLength;
    for (let i = 0; i < len; i++) {
        ret += String.fromCharCode(bytes[i]);
    }
    return window.btoa(ret);
}

function bufFromBase64Native(x: string): ArrayBuffer | null {
    try {
        // @ts-ignore:next-line
        return Uint8Array.fromBase64(x).buffer;
    } catch {
        return null;
    }
}

function bufFromBase64Slow(x: string): ArrayBuffer | null {
    try {
        return new Uint8Array(Array.from(window.atob(x), x => x.charCodeAt(0))).buffer;
    } catch {
        return null;
    }
}

// @ts-ignore:next-line
export const bufToBase64 = Uint8Array.prototype.toBase64 === undefined ? bufToBase64Slow : bufToBase64Native;

// @ts-ignore:next-line
export const bufFromBase64 = Uint8Array.fromBase64 === undefined ? bufFromBase64Slow : bufFromBase64Native;
