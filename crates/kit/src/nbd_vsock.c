/*
 * nbd-vsock: connect NBD device to server via AF_VSOCK using netlink API.
 *
 * Usage: nbd-vsock /dev/nbdN vsock_port [num_connections]
 *
 * Listens on AF_VSOCK port and waits for Host relay to connect (Host-initiated
 * connection for better hv_sock throughput). Performs NBD handshake, creates
 * AF_UNIX socketpairs, hands unix FDs to the kernel via netlink NBD_CMD_CONNECT,
 * and relays data between vsock and unix sockets in userspace.
 *
 * This approach uses standard (unpatched) nbd.ko — no AF_VSOCK kernel patch needed.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <linux/vm_sockets.h>
#include <linux/netlink.h>
#include <linux/genetlink.h>
#include <stdint.h>
#include <endian.h>
#include <pthread.h>
#include <signal.h>

#define NBD_GENL_FAMILY_NAME "nbd"
#define NBD_GENL_VERSION     0x1
#define MAX_CONNECTIONS      16
#define RELAY_BUF_SIZE       (256 * 1024)

enum { NBD_CMD_UNSPEC, NBD_CMD_CONNECT, NBD_CMD_DISCONNECT, NBD_CMD_RECONFIGURE,
       NBD_CMD_LINK_DEAD, NBD_CMD_STATUS };
enum { NBD_ATTR_UNSPEC, NBD_ATTR_INDEX, NBD_ATTR_SIZE_BYTES,
       NBD_ATTR_BLOCK_SIZE_BYTES, NBD_ATTR_TIMEOUT, NBD_ATTR_SERVER_FLAGS,
       NBD_ATTR_CLIENT_FLAGS, NBD_ATTR_SOCKETS };
enum { NBD_SOCK_ITEM_UNSPEC, NBD_SOCK_ITEM };
enum { NBD_SOCK_UNSPEC, NBD_SOCK_FD };

struct relay_args {
    int from_fd;
    int to_fd;
};

static int readall(int fd, void *buf, size_t n) {
    size_t done = 0;
    while (done < n) {
        ssize_t r = read(fd, (char *)buf + done, n - done);
        if (r <= 0) return -1;
        done += r;
    }
    return 0;
}

static int writeall(int fd, const void *buf, size_t n) {
    size_t done = 0;
    while (done < n) {
        ssize_t r = write(fd, (const char *)buf + done, n - done);
        if (r <= 0) return -1;
        done += r;
    }
    return 0;
}

/* ---- sd_notify (no libsystemd dependency) ---- */

static void sd_notify_ready(void) {
    const char *sock_path = getenv("NOTIFY_SOCKET");
    if (!sock_path) return;
    int fd = socket(AF_UNIX, SOCK_DGRAM, 0);
    if (fd < 0) return;
    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    if (sock_path[0] == '@') {
        addr.sun_path[0] = '\0';
        strncpy(addr.sun_path + 1, sock_path + 1, sizeof(addr.sun_path) - 2);
    } else {
        strncpy(addr.sun_path, sock_path, sizeof(addr.sun_path) - 1);
    }
    socklen_t len = offsetof(struct sockaddr_un, sun_path) + strlen(sock_path);
    if (sock_path[0] == '@') len = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(sock_path + 1);
    sendto(fd, "READY=1", 7, 0, (struct sockaddr *)&addr, len);
    close(fd);
}

/* ---- relay thread ---- */

static void *relay_thread(void *arg) {
    struct relay_args *ra = (struct relay_args *)arg;
    char *buf = malloc(RELAY_BUF_SIZE);
    if (!buf) { free(ra); return NULL; }

    while (1) {
        ssize_t n = read(ra->from_fd, buf, RELAY_BUF_SIZE);
        if (n <= 0) break;
        if (writeall(ra->to_fd, buf, n) < 0) break;
    }

    shutdown(ra->from_fd, SHUT_RD);
    shutdown(ra->to_fd, SHUT_WR);
    free(buf);
    free(ra);
    return NULL;
}

static void start_relay(int fd_a, int fd_b, pthread_t *t1, pthread_t *t2) {
    struct relay_args *a2b = malloc(sizeof(*a2b));
    struct relay_args *b2a = malloc(sizeof(*b2a));
    a2b->from_fd = fd_a; a2b->to_fd = fd_b;
    b2a->from_fd = fd_b; b2a->to_fd = fd_a;
    pthread_create(t1, NULL, relay_thread, a2b);
    pthread_create(t2, NULL, relay_thread, b2a);
}

/* ---- NLA builder ---- */

static void nla_put_u32(char *buf, int *pos, uint16_t type, uint32_t val) {
    struct nlattr { uint16_t nla_len; uint16_t nla_type; } hdr;
    hdr.nla_len = 4 + 4; hdr.nla_type = type;
    memcpy(buf + *pos, &hdr, 4); *pos += 4;
    memcpy(buf + *pos, &val, 4); *pos += 4;
}

static void nla_put_u64(char *buf, int *pos, uint16_t type, uint64_t val) {
    struct nlattr { uint16_t nla_len; uint16_t nla_type; } hdr;
    hdr.nla_len = 4 + 8; hdr.nla_type = type;
    memcpy(buf + *pos, &hdr, 4); *pos += 4;
    memcpy(buf + *pos, &val, 8); *pos += 8;
}

static void nla_put_string(char *buf, int *pos, uint16_t type, const char *s) {
    struct nlattr { uint16_t nla_len; uint16_t nla_type; } hdr;
    int slen = strlen(s) + 1;
    hdr.nla_len = 4 + slen; hdr.nla_type = type;
    memcpy(buf + *pos, &hdr, 4); *pos += 4;
    memcpy(buf + *pos, s, slen);
    *pos += NLA_ALIGN(slen);
}

static int nla_nest_start(char *buf, int *pos, uint16_t type) {
    struct nlattr { uint16_t nla_len; uint16_t nla_type; } hdr;
    int start = *pos;
    hdr.nla_len = 0; hdr.nla_type = type | NLA_F_NESTED;
    memcpy(buf + *pos, &hdr, 4); *pos += 4;
    return start;
}

static void nla_nest_end(char *buf, int start, int *pos) {
    uint16_t len = *pos - start;
    memcpy(buf + start, &len, 2);
}

/* ---- netlink ---- */

static int nl_open(void) {
    int fd = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_GENERIC);
    if (fd < 0) return -1;
    struct sockaddr_nl sa = { .nl_family = AF_NETLINK, .nl_pid = getpid() };
    if (bind(fd, (struct sockaddr *)&sa, sizeof(sa)) < 0) { close(fd); return -1; }
    return fd;
}

static int genl_resolve_family(int nl, const char *name) {
    char buf[4096]; int pos = 0;
    struct nlmsghdr *nh = (struct nlmsghdr *)buf;
    pos = sizeof(struct nlmsghdr);
    struct genlmsghdr gh = { .cmd = CTRL_CMD_GETFAMILY, .version = 1 };
    memcpy(buf + pos, &gh, sizeof(gh)); pos += sizeof(gh);
    nla_put_string(buf, &pos, CTRL_ATTR_FAMILY_NAME, name);
    nh->nlmsg_len = pos; nh->nlmsg_type = GENL_ID_CTRL;
    nh->nlmsg_flags = NLM_F_REQUEST; nh->nlmsg_seq = 1; nh->nlmsg_pid = getpid();
    if (send(nl, buf, pos, 0) < 0) return -1;
    int n = recv(nl, buf, sizeof(buf), 0);
    if (n < 0) return -1;
    nh = (struct nlmsghdr *)buf;
    if (nh->nlmsg_type == NLMSG_ERROR) {
        int *err = (int *)(buf + sizeof(struct nlmsghdr));
        if (*err) { errno = -*err; return -1; }
    }
    int off = sizeof(struct nlmsghdr) + sizeof(struct genlmsghdr);
    while (off + 4 <= n) {
        uint16_t alen, atype;
        memcpy(&alen, buf + off, 2); memcpy(&atype, buf + off + 2, 2);
        if (alen < 4) break;
        if (atype == CTRL_ATTR_FAMILY_ID && alen >= 6) {
            uint16_t fid; memcpy(&fid, buf + off + 4, 2); return fid;
        }
        off += NLA_ALIGN(alen);
    }
    errno = ENOENT; return -1;
}

static int nbd_connect_netlink_multi(int nl, int family_id, int dev_index,
                                     int *sock_fds, int num_socks,
                                     uint64_t size, uint64_t blksize,
                                     uint64_t server_flags) {
    char buf[4096]; int pos = 0;
    struct nlmsghdr *nh = (struct nlmsghdr *)buf;
    pos = sizeof(struct nlmsghdr);
    struct genlmsghdr gh = { .cmd = NBD_CMD_CONNECT, .version = NBD_GENL_VERSION };
    memcpy(buf + pos, &gh, sizeof(gh)); pos += sizeof(gh);
    nla_put_u32(buf, &pos, NBD_ATTR_INDEX, dev_index);
    nla_put_u64(buf, &pos, NBD_ATTR_SIZE_BYTES, size);
    nla_put_u64(buf, &pos, NBD_ATTR_BLOCK_SIZE_BYTES, blksize);
    nla_put_u64(buf, &pos, NBD_ATTR_TIMEOUT, 0);
    nla_put_u64(buf, &pos, NBD_ATTR_SERVER_FLAGS, server_flags);
    int socks_start = nla_nest_start(buf, &pos, NBD_ATTR_SOCKETS);
    for (int i = 0; i < num_socks; i++) {
        int item_start = nla_nest_start(buf, &pos, NBD_SOCK_ITEM);
        nla_put_u32(buf, &pos, NBD_SOCK_FD, sock_fds[i]);
        nla_nest_end(buf, item_start, &pos);
    }
    nla_nest_end(buf, socks_start, &pos);
    nh->nlmsg_len = pos; nh->nlmsg_type = family_id;
    nh->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK; nh->nlmsg_seq = 2; nh->nlmsg_pid = getpid();
    if (send(nl, buf, pos, 0) < 0) return -1;
    int n = recv(nl, buf, sizeof(buf), 0);
    if (n < 0) return -1;
    nh = (struct nlmsghdr *)buf;
    if (nh->nlmsg_type == NLMSG_ERROR) {
        int *err = (int *)(buf + sizeof(struct nlmsghdr));
        if (*err) {
            fprintf(stderr, "nbd-vsock: NBD_CMD_CONNECT error: %s\n", strerror(-*err));
            return -1;
        }
    }
    return 0;
}

/* ---- vsock listen + accept + NBD handshake ---- */

static int vsock_listen(unsigned int port) {
    int lsock = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (lsock < 0) { perror("vsock socket"); return -1; }
    int opt = 1;
    setsockopt(lsock, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));
    struct sockaddr_vm sa = { .svm_family = AF_VSOCK, .svm_cid = VMADDR_CID_ANY, .svm_port = port };
    if (bind(lsock, (struct sockaddr *)&sa, sizeof(sa)) < 0) {
        perror("vsock bind"); close(lsock); return -1;
    }
    if (listen(lsock, MAX_CONNECTIONS) < 0) {
        perror("vsock listen"); close(lsock); return -1;
    }
    fprintf(stderr, "nbd-vsock: listening on vsock port %u\n", port);
    return lsock;
}

static int vsock_accept_and_handshake(int lsock, uint64_t *out_size, uint16_t *out_flags) {
    int sock = accept(lsock, NULL, NULL);
    if (sock < 0) { perror("vsock accept"); return -1; }

    uint64_t magic, ihaveopt;
    uint16_t hflags;
    if (readall(sock, &magic, 8) < 0 || readall(sock, &ihaveopt, 8) < 0 ||
        readall(sock, &hflags, 2) < 0) {
        fprintf(stderr, "nbd-vsock: handshake read failed\n"); close(sock); return -1;
    }
    ihaveopt = be64toh(ihaveopt); hflags = be16toh(hflags);

    uint32_t cflags = htobe32(1);
    writeall(sock, &cflags, 4);
    uint64_t opt_magic = htobe64(0x49484156454F5054ULL);
    uint32_t opt_id = htobe32(1);
    uint32_t opt_len = htobe32(0);
    writeall(sock, &opt_magic, 8);
    writeall(sock, &opt_id, 4);
    writeall(sock, &opt_len, 4);

    uint64_t export_size;
    uint16_t tflags;
    char pad[124];
    if (readall(sock, &export_size, 8) < 0 || readall(sock, &tflags, 2) < 0 ||
        readall(sock, pad, 124) < 0) {
        fprintf(stderr, "nbd-vsock: export info read failed\n"); close(sock); return -1;
    }
    *out_size = be64toh(export_size);
    *out_flags = be16toh(tflags);
    return sock;
}

/* ---- main ---- */

int main(int argc, char **argv) {
    if (argc < 3 || argc > 4) {
        fprintf(stderr, "Usage: nbd-vsock /dev/nbdX vsock_port [num_connections]\n");
        return 1;
    }
    const char *dev = argv[1];
    unsigned int port = atoi(argv[2]);
    int num_conns = (argc >= 4) ? atoi(argv[3]) : 1;
    if (num_conns < 1) num_conns = 1;
    if (num_conns > MAX_CONNECTIONS) num_conns = MAX_CONNECTIONS;
    int dev_index = atoi(dev + strlen("/dev/nbd"));

    signal(SIGPIPE, SIG_IGN);

    int lsock = vsock_listen(port);
    if (lsock < 0) return 1;

    fprintf(stderr, "nbd-vsock: waiting for %d connection(s) from Host relay\n", num_conns);

    int vsock_fds[MAX_CONNECTIONS];
    int unix_fds[MAX_CONNECTIONS];
    pthread_t relay_threads[MAX_CONNECTIONS * 2];
    uint64_t export_size = 0;
    uint16_t tflags = 0;

    for (int i = 0; i < num_conns; i++) {
        vsock_fds[i] = vsock_accept_and_handshake(lsock, &export_size, &tflags);
        if (vsock_fds[i] < 0) {
            fprintf(stderr, "nbd-vsock: connection %d failed\n", i);
            return 1;
        }

        int sockbuf = 1024 * 1024;
        setsockopt(vsock_fds[i], SOL_SOCKET, SO_SNDBUF, &sockbuf, sizeof(sockbuf));
        setsockopt(vsock_fds[i], SOL_SOCKET, SO_RCVBUF, &sockbuf, sizeof(sockbuf));

        int pair[2];
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) < 0) {
            perror("socketpair");
            return 1;
        }

        int sndbuf = RELAY_BUF_SIZE;
        setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &sndbuf, sizeof(sndbuf));
        setsockopt(pair[0], SOL_SOCKET, SO_RCVBUF, &sndbuf, sizeof(sndbuf));
        setsockopt(pair[1], SOL_SOCKET, SO_SNDBUF, &sndbuf, sizeof(sndbuf));
        setsockopt(pair[1], SOL_SOCKET, SO_RCVBUF, &sndbuf, sizeof(sndbuf));

        unix_fds[i] = pair[0];
        start_relay(vsock_fds[i], pair[1], &relay_threads[i * 2], &relay_threads[i * 2 + 1]);
    }
    close(lsock);

    fprintf(stderr, "nbd-vsock: %d connection(s), export size=%llu bytes\n",
            num_conns, (unsigned long long)export_size);

    int nl = nl_open();
    if (nl < 0) { perror("netlink socket"); return 1; }
    int family_id = genl_resolve_family(nl, NBD_GENL_FAMILY_NAME);
    if (family_id < 0) {
        fprintf(stderr, "nbd-vsock: nbd genl family: %s\n", strerror(errno)); return 1;
    }

    if (nbd_connect_netlink_multi(nl, family_id, dev_index, unix_fds, num_conns,
                                  export_size, 512, tflags) < 0) {
        fprintf(stderr, "nbd-vsock: NBD_CMD_CONNECT failed\n"); return 1;
    }

    fprintf(stderr, "nbd-vsock: kernel I/O started, relay running\n");
    close(nl);
    sd_notify_ready();

    /* Survive initrd → rootfs switch_root:
     * 1. Ignore SIGTERM so systemd's cleanup doesn't kill us
     * 2. Move to root cgroup so service cgroup cleanup doesn't SIGKILL us */
    signal(SIGTERM, SIG_IGN);
    {
        char pid_str[32];
        snprintf(pid_str, sizeof(pid_str), "%d\n", getpid());
        int cgfd = open("/sys/fs/cgroup/cgroup.procs", O_WRONLY);
        if (cgfd >= 0) { write(cgfd, pid_str, strlen(pid_str)); close(cgfd); }
    }

    for (int i = 0; i < num_conns * 2; i++)
        pthread_join(relay_threads[i], NULL);

    return 0;
}
