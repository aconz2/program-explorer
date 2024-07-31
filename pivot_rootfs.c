/*
 * Do the thing in https://github.com/containers/bubblewrap/issues/592#issuecomment-2243087731
 * unshare --mount
 * mount --rbind / /abc --mkdir
 * cd /abc
 * mount --move . /
 * chroot .
 */


#include <unistd.h>
#include <stdlib.h>
#include <stdio.h>

int main(int argc, char** argv) {
    if (argc < 2) {
        fputs("must supply a program to run\n", stderr);
        exit(EXIT_FAILURE);
    }
    execvp(argv[1], &argv[2]);
    perror("should not get here");
    exit(EXIT_FAILURE);
}
