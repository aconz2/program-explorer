#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <linux/vm_sockets.h>


// <location> <fd> [...]
// location is v<port> or u<path>
// fd is 0 | 1
int main(int argc, char **argv)
{
    if (argc < 3) {
        fputs("<location> <fd>\n", stderr);
        exit(EXIT_FAILURE);
    }
    const char* location = argv[1];

    int fd = atoi(argv[2]);
    if (!((fd == 0) || (fd == 1))) {
        fputs("<fd> must be 0 or 1\n", stderr);
        exit(EXIT_FAILURE);
    }

    int sock;
    int ret;
    int dupfd;

    if (location[0] == 'u') {
        struct sockaddr_un addr;
        memset(&addr, 0, sizeof(addr));
        addr.sun_family = AF_UNIX;
        strncpy(addr.sun_path, &location[1], strlen(&location[1]));
        sock = socket(AF_UNIX, SOCK_STREAM, 0);
        if (sock < 0) {perror("socket"); exit(EXIT_FAILURE);}
        ret = bind(sock, (struct sockaddr *)&addr, sizeof(addr));
        if (ret < 0) {perror("bind"); exit(EXIT_FAILURE);}
        ret = listen(sock, 0);
        if (ret < 0) {perror("listen"); exit(EXIT_FAILURE);}
        dupfd = accept(sock, NULL, 0);
        if (dupfd < 0) {perror("accept"); exit(EXIT_FAILURE);}
        ret = close(sock);
        if (ret < 0) {perror("close sock"); exit(EXIT_FAILURE);}

    } else if (location[0] == 'v') {
        int port = atoi(&location[1]);
        struct sockaddr_vm addr;
        memset(&addr, 0, sizeof(addr));
        addr.svm_family = AF_VSOCK;
        addr.svm_reserved1 = 0;
        addr.svm_cid = VMADDR_CID_HOST;
        addr.svm_port = port;
        sock = socket(AF_VSOCK, SOCK_STREAM, 0);
        if (sock < 0) {perror("socket"); exit(EXIT_FAILURE);}
        ret = connect(sock, (struct sockaddr *)&addr, sizeof(addr));
        if (ret < 0) {perror("connect"); exit(EXIT_FAILURE);}
        dupfd = sock;
    } else {
        fputs("<location> must be u or v\n", stderr);
        exit(EXIT_FAILURE);
    }

    ret = close(fd);
    if (ret < 0) {perror("close fd"); exit(EXIT_FAILURE);}

    ret = dup2(dupfd, fd);
    if (ret < 0) {perror("dup2"); exit(EXIT_FAILURE);}

    if (argc >= 4) {
        ret = execvp(argv[3], &argv[3]);
        if (ret < 0) {perror("execvp"); exit(EXIT_FAILURE);}
    }
    return 0;
}
