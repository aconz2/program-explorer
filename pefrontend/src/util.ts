
export function bufToHex(data: ArrayBuffer, length=100): string {
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

export function parseEnvText(s: string): [string] {
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
