/*
 * Do the thing in https://github.com/containers/bubblewrap/issues/592#issuecomment-2243087731
 * unshare --mount
 * mount --rbind / /abc --mkdir
 * cd /abc
 * mount --move . /
 * chroot .
 */

#define _GNU_SOURCE
#include <unistd.h>
#include <stdlib.h>
#include <stdio.h>
#include <sched.h>
#include <sys/mount.h>

int main(int argc, char** argv) {
    if (argc < 3) {
        fputs("args: <dir> <program> ...\n", stderr);
        exit(EXIT_FAILURE);
    }

    if (unshare(CLONE_NEWNS) < 0) {
        perror("unshare --mount");
        exit(EXIT_FAILURE);
    }

    if (mount("/", argv[1], NULL, MS_BIND | MS_REC | MS_SILENT, NULL) < 0) {
        perror("mount --rbind / <dir>");
        exit(EXIT_FAILURE);
    }

    // note: this can't be an fchdir with a dirfd opened previous to the mount
    if (chdir(argv[1]) < 0) {
        perror("fchdir dirfd");
        exit(EXIT_FAILURE);
    }

    if (mount(argv[1], "/", NULL, MS_MOVE | MS_SILENT, NULL) < 0) {
        perror("mount --move . /");
        exit(EXIT_FAILURE);
    }

    if (chroot(".") < 0) {
        perror("chroot .");
        exit(EXIT_FAILURE);
    }

    // this is not necessary though chroot(1) does do this
    // if (chdir("/") < 0) {
    //     perror("chdir /");
    //     exit(EXIT_FAILURE);
    // }

    if (setuid(1000) < 0) {
        perror("setuid");
        exit(EXIT_FAILURE);
    }

    if (execvp(argv[2], &argv[2]) < 0) {
        perror("execvp");
        exit(EXIT_FAILURE);
    }

    return 1;
}
