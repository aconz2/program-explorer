export namespace Api {
    export type Siginfo =
          {Exited: number}
        | {Killed: number}
        | {Dumped: number}
        | {Stopped: number}
        | {Trapped: number}
        | {Continued: number};

    export type TimeVal = {
        sec: number, // TODO these are i64 so maybe blow up in json
        usec: number,
    };

    export type Rusage = {
        ru_utime    : TimeVal,     /* user CPU time used */
        ru_stime    : TimeVal,     /* system CPU time used */
        ru_maxrss   : number,      /* maximum resident set size */
        ru_ixrss    : number,      /* integral shared memory size */
        ru_idrss    : number,      /* integral unshared data size */
        ru_isrss    : number,      /* integral unshared stack size */
        ru_minflt   : number,      /* page reclaims (soft page faults) */
        ru_majflt   : number,      /* page faults (hard page faults) */
        ru_nswap    : number,      /* swaps */
        ru_inblock  : number,      /* block input operations */
        ru_oublock  : number,      /* block output operations */
        ru_msgsnd   : number,      /* IPC messages sent */
        ru_msgrcv   : number,      /* IPC messages received */
        ru_nsignals : number,      /* signals received */
        ru_nvcsw    : number,      /* voluntary context switches */
        ru_nivcsw   : number,      /* involuntary context switches */
    };

    export namespace Runi {
        export type Request = {
            stdin?: string,
            entrypoint?: string[],
            cmd?: string[],
        };
        export type Response =
            | {Ok: {siginfo: Siginfo, rusage: Rusage}}
            | {Overtime: {siginfo: Siginfo, rusage: Rusage}}
            | {Panic: {message: string}};
    }

    export type Image = {
        links: {
            runi: string,
            upstream: string,
        },
        info: {
            digest: string,
            repository: string,
            registry: string,
            tag: string,
        },
        config: {
            created: string,
            architecture: string,
            os: string,
            config: {
                Cmd?: string[],
                Entrypoint?: string[],
                Env?: string[],
            },
            rootfs: {type: string, diff_ids: string[]}[],
            history: any, // todo
        },
    };
}
