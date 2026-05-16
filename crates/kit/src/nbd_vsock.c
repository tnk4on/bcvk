/*
 * nbd-tcp: connect NBD device to server via TCP using netlink API.
 *
 * Usage: nbd-tcp /dev/nbdN host port
 *
 * The netlink generic API hands the TCP socket to the kernel's internal
 * NBD I/O threads, which survive dracut switch-root.
 * TCP works with stock nbd.ko — no kernel patch needed.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <linux/netlink.h>
#include <linux/genetlink.h>
#include <stdint.h>
#include <endian.h>

/* NBD netlink constants (from linux/nbd-netlink.h, inlined for portability) */
#define NBD_GENL_FAMILY_NAME "nbd"
#define NBD_GENL_VERSION     0x1

enum { NBD_CMD_UNSPEC, NBD_CMD_CONNECT, NBD_CMD_DISCONNECT, NBD_CMD_RECONFIGURE,
       NBD_CMD_LINK_DEAD, NBD_CMD_STATUS };
enum { NBD_ATTR_UNSPEC, NBD_ATTR_INDEX, NBD_ATTR_SIZE_BYTES,
       NBD_ATTR_BLOCK_SIZE_BYTES, NBD_ATTR_TIMEOUT, NBD_ATTR_SERVER_FLAGS,
       NBD_ATTR_CLIENT_FLAGS, NBD_ATTR_SOCKETS };
enum { NBD_SOCK_ITEM_UNSPEC, NBD_SOCK_ITEM };
enum { NBD_SOCK_UNSPEC, NBD_SOCK_FD };

/* NLA_F_NESTED and NLA_ALIGN come from <linux/netlink.h> */

/* ---- helpers ---- */

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

/* ---- NLA builder ---- */

static void nla_put_u32(char *buf, int *pos, uint16_t type, uint32_t val) {
    struct nlattr { uint16_t nla_len; uint16_t nla_type; } hdr;
    hdr.nla_len = 4 + 4;
    hdr.nla_type = type;
    memcpy(buf + *pos, &hdr, 4); *pos += 4;
    memcpy(buf + *pos, &val, 4); *pos += 4;
}

static void nla_put_u64(char *buf, int *pos, uint16_t type, uint64_t val) {
    struct nlattr { uint16_t nla_len; uint16_t nla_type; } hdr;
    hdr.nla_len = 4 + 8;
    hdr.nla_type = type;
    memcpy(buf + *pos, &hdr, 4); *pos += 4;
    memcpy(buf + *pos, &val, 8); *pos += 8;
}

static void nla_put_string(char *buf, int *pos, uint16_t type, const char *s) {
    struct nlattr { uint16_t nla_len; uint16_t nla_type; } hdr;
    int slen = strlen(s) + 1;
    hdr.nla_len = 4 + slen;
    hdr.nla_type = type;
    memcpy(buf + *pos, &hdr, 4); *pos += 4;
    memcpy(buf + *pos, s, slen);
    *pos += NLA_ALIGN(slen);
}

static int nla_nest_start(char *buf, int *pos, uint16_t type) {
    struct nlattr { uint16_t nla_len; uint16_t nla_type; } hdr;
    int start = *pos;
    hdr.nla_len = 0;
    hdr.nla_type = type | NLA_F_NESTED;
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
    if (bind(fd, (struct sockaddr *)&sa, sizeof(sa)) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int genl_resolve_family(int nl, const char *name) {
    char buf[4096];
    int pos = 0;

    struct nlmsghdr *nh = (struct nlmsghdr *)buf;
    pos = sizeof(struct nlmsghdr);

    struct genlmsghdr gh = { .cmd = CTRL_CMD_GETFAMILY, .version = 1 };
    memcpy(buf + pos, &gh, sizeof(gh)); pos += sizeof(gh);

    nla_put_string(buf, &pos, CTRL_ATTR_FAMILY_NAME, name);

    nh->nlmsg_len = pos;
    nh->nlmsg_type = GENL_ID_CTRL;
    nh->nlmsg_flags = NLM_F_REQUEST;
    nh->nlmsg_seq = 1;
    nh->nlmsg_pid = getpid();

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
        memcpy(&alen, buf + off, 2);
        memcpy(&atype, buf + off + 2, 2);
        if (alen < 4) break;
        if (atype == CTRL_ATTR_FAMILY_ID && alen >= 6) {
            uint16_t fid;
            memcpy(&fid, buf + off + 4, 2);
            return fid;
        }
        off += NLA_ALIGN(alen);
    }
    errno = ENOENT;
    return -1;
}

static int nbd_connect_netlink(int nl, int family_id, int dev_index,
                               int sock_fd, uint64_t size, uint64_t blksize,
                               uint64_t server_flags) {
    char buf[4096];
    int pos = 0;

    struct nlmsghdr *nh = (struct nlmsghdr *)buf;
    pos = sizeof(struct nlmsghdr);

    struct genlmsghdr gh = { .cmd = NBD_CMD_CONNECT, .version = NBD_GENL_VERSION };
    memcpy(buf + pos, &gh, sizeof(gh)); pos += sizeof(gh);

    nla_put_u32(buf, &pos, NBD_ATTR_INDEX, dev_index);
    nla_put_u64(buf, &pos, NBD_ATTR_SIZE_BYTES, size);
    nla_put_u64(buf, &pos, NBD_ATTR_BLOCK_SIZE_BYTES, blksize);
    nla_put_u64(buf, &pos, NBD_ATTR_SERVER_FLAGS, server_flags);

    int socks_start = nla_nest_start(buf, &pos, NBD_ATTR_SOCKETS);
    int item_start = nla_nest_start(buf, &pos, NBD_SOCK_ITEM);
    nla_put_u32(buf, &pos, NBD_SOCK_FD, sock_fd);
    nla_nest_end(buf, item_start, &pos);
    nla_nest_end(buf, socks_start, &pos);

    nh->nlmsg_len = pos;
    nh->nlmsg_type = family_id;
    nh->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    nh->nlmsg_seq = 2;
    nh->nlmsg_pid = getpid();

    if (send(nl, buf, pos, 0) < 0) return -1;

    int n = recv(nl, buf, sizeof(buf), 0);
    if (n < 0) return -1;

    nh = (struct nlmsghdr *)buf;
    if (nh->nlmsg_type == NLMSG_ERROR) {
        int *err = (int *)(buf + sizeof(struct nlmsghdr));
        if (*err) {
            fprintf(stderr, "nbd-tcp: NBD_CMD_CONNECT error: %s\n", strerror(-*err));
            return -1;
        }
    }
    return 0;
}

/* ---- main ---- */

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "Usage: nbd-tcp /dev/nbdX host port\n");
        return 1;
    }
    const char *dev = argv[1];
    const char *host = argv[2];
    unsigned int port = atoi(argv[3]);

    int dev_index = atoi(dev + strlen("/dev/nbd"));

    /* TCP connect */
    int sock = socket(AF_INET, SOCK_STREAM, 0);
    if (sock < 0) { perror("tcp socket"); return 1; }
    struct sockaddr_in sa = {0};
    sa.sin_family = AF_INET;
    sa.sin_port = htons(port);
    if (inet_pton(AF_INET, host, &sa.sin_addr) <= 0) {
        fprintf(stderr, "nbd-tcp: invalid host: %s\n", host);
        return 1;
    }
    fprintf(stderr, "nbd-tcp: connecting %s:%u\n", host, port);
    if (connect(sock, (struct sockaddr *)&sa, sizeof(sa)) < 0) {
        perror("tcp connect"); return 1;
    }
    fprintf(stderr, "nbd-tcp: connected\n");

    /* NBD newstyle-fixed handshake */
    uint64_t magic, ihaveopt;
    uint16_t hflags;
    if (readall(sock, &magic, 8) < 0 || readall(sock, &ihaveopt, 8) < 0 ||
        readall(sock, &hflags, 2) < 0) {
        fprintf(stderr, "nbd-tcp: handshake read failed\n"); return 1;
    }
    magic = be64toh(magic); ihaveopt = be64toh(ihaveopt); hflags = be16toh(hflags);
    fprintf(stderr, "nbd-tcp: magic=%llx flags=%x\n",
            (unsigned long long)ihaveopt, hflags);

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
        fprintf(stderr, "nbd-tcp: export info read failed\n"); return 1;
    }
    export_size = be64toh(export_size);
    tflags = be16toh(tflags);
    fprintf(stderr, "nbd-tcp: export size=%llu bytes, flags=%x\n",
            (unsigned long long)export_size, tflags);

    /* netlink: connect NBD device */
    int nl = nl_open();
    if (nl < 0) { perror("netlink socket"); return 1; }

    int family_id = genl_resolve_family(nl, NBD_GENL_FAMILY_NAME);
    if (family_id < 0) {
        fprintf(stderr, "nbd-tcp: failed to resolve nbd genl family: %s\n",
                strerror(errno));
        return 1;
    }
    fprintf(stderr, "nbd-tcp: nbd genl family_id=%d\n", family_id);

    if (nbd_connect_netlink(nl, family_id, dev_index, sock,
                            export_size, 512, tflags) < 0) {
        fprintf(stderr, "nbd-tcp: netlink NBD_CMD_CONNECT failed\n");
        return 1;
    }

    fprintf(stderr, "nbd-tcp: kernel I/O threads started, exiting\n");
    close(nl);
    return 0;
}
