import {parse} from 'toml';

export type PeToml = {
    env: string | null,
    cmd: string | null,
    image: string | null,
    stdin: string | null,
};

export function parsePeToml(s: string): PeToml {
    let parsed = parse(s);
    return {
        env: parsed.env ?? null,
        cmd: parsed.cmd ?? null,
        image: parsed.image ?? null,
        stdin: parsed.stdin ?? null,
    };
}
