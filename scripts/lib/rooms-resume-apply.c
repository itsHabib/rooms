#define _GNU_SOURCE

/*
 * Post-restore guest hygiene for snapshot clones.
 *
 * This process is deliberately static and small. It starts at boot and owns
 * the reconnect loop itself, so this exact process is retained in the neutral
 * snapshot. A clone therefore performs no post-resume ELF exec (and no
 * AT_RANDOM draw) before the host entropy is mixed and the kernel CRNG is
 * forcibly reseeded. Only then may it fork a fresh tool or daemon.
 */

#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <limits.h>
#include <poll.h>
#include <pwd.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifdef __linux__
#include <linux/random.h>
#include <linux/vm_sockets.h>
#include <sys/ioctl.h>
#endif

#ifndef PATH_MAX
#define PATH_MAX 4096
#endif

#define ROOM_ID_LEN 26U
#define ENTROPY_LEN 64U
#define MAX_SECRETS_LEN (1024U * 1024U)
#define MAX_CONFIG_LEN (1024U * 1024U)
#define MAX_LINE_LEN 1024U
#define LISTEN_WAIT_NS 10000000L
#define LISTEN_WAIT_TRIES 200U
#define RESUME_HOST_CID 2U
#define RESUME_PORT 5003U
#define RESUME_CONNECT_MS 100
#define RESUME_IO_TIMEOUT_SECONDS 120
#define PROTOCOL_ERROR_LINE_MAX 64U

static const char sudo_grant[] = "rooms ALL=(ALL) NOPASSWD: ALL\n";
static const char repo_include[] = "\n[include]\n\tpath = rooms-identity\n";
static const char canonical_sshd_config[] =
    "Port 22\n"
    "AddressFamily inet\n"
    "ListenAddress 0.0.0.0\n"
    "HostKey /etc/ssh/ssh_host_ed25519_key\n"
    "PidFile /run/sshd.pid\n"
    "PermitRootLogin no\n"
    "PubkeyAuthentication yes\n"
    "AuthenticationMethods publickey\n"
    "PasswordAuthentication no\n"
    "KbdInteractiveAuthentication no\n"
    "PermitEmptyPasswords no\n"
    "UseDNS no\n"
    "AllowUsers rooms\n"
    "AuthorizedKeysFile .ssh/authorized_keys\n"
    "StrictModes yes\n"
    "AllowAgentForwarding no\n"
    "AllowTcpForwarding no\n"
    "X11Forwarding no\n"
    "PermitTunnel no\n"
    "PermitUserEnvironment no\n"
    "PrintMotd no\n"
    "AcceptEnv ANTHROPIC_API_KEY\n"
    "AcceptEnv CLAUDE_CODE_OAUTH_TOKEN\n"
    "AcceptEnv ANTHROPIC_AUTH_TOKEN\n"
    "Subsystem sftp internal-sftp\n";

struct payload {
    char room_id[ROOM_ID_LEN + 1U];
    time_t epoch;
    unsigned char entropy[ENTROPY_LEN];
    unsigned char *secrets;
    size_t secrets_len;
};

struct paths {
    char run_rooms[PATH_MAX];
    char identity[PATH_MAX];
    char secrets_env[PATH_MAX];
    char rooms_home[PATH_MAX];
    char global_git[PATH_MAX];
    char workspace[PATH_MAX];
    char repo[PATH_MAX];
    char repo_git[PATH_MAX];
    char repo_config[PATH_MAX];
    char repo_worktree_config[PATH_MAX];
    char repo_identity[PATH_MAX];
    char ssh_dir[PATH_MAX];
    char ssh_key[PATH_MAX];
    char ssh_pub[PATH_MAX];
    char sshd_config[PATH_MAX];
    char sshd_runtime[PATH_MAX];
    char sshd_pid[PATH_MAX];
    char sudoers_dir[PATH_MAX];
    char sudoers_file[PATH_MAX];
    char sudoers[PATH_MAX];
};

static struct payload sensitive_payload;
static bool resume_stream_attached;
static const char *protocol_stage = "handshake";

static void scrub(void *data, size_t length)
{
    volatile unsigned char *bytes = data;
    while (length > 0U) {
        *bytes = 0U;
        bytes++;
        length--;
    }
}

static void scrub_payload(void)
{
    scrub(sensitive_payload.entropy, sizeof(sensitive_payload.entropy));
    if (sensitive_payload.secrets != NULL) {
        scrub(sensitive_payload.secrets, sensitive_payload.secrets_len);
        free(sensitive_payload.secrets);
        sensitive_payload.secrets = NULL;
    }
    sensitive_payload.secrets_len = 0U;
}

static int write_all(int fd, const void *data, size_t length)
{
    const unsigned char *cursor = data;
    while (length > 0U) {
        ssize_t written = write(fd, cursor, length);
        if (written < 0 && errno == EINTR) {
            continue;
        }
        if (written <= 0) {
            return -1;
        }
        cursor += (size_t)written;
        length -= (size_t)written;
    }
    return 0;
}

static void set_protocol_stage(const char *stage)
{
    protocol_stage = stage;
}

static void send_protocol_error(const char *message)
{
    if (!resume_stream_attached) return;

    /* read_bounded_line accepts at most 64 bytes before the newline. Keep the
     * complete guest error record within that bound even when a defensive
     * check carries a long path or diagnostic. Delivery is best effort: the
     * transport itself may be the failing component. */
    char line[PROTOCOL_ERROR_LINE_MAX + 1U];
    int count = snprintf(line, sizeof(line), "ERR %.12s %.46s\n",
                         protocol_stage, message);
    if (count <= 0 || (size_t)count >= sizeof(line)) return;
    (void)write_all(STDOUT_FILENO, line, (size_t)count);
}

static int fail(const char *message)
{
    dprintf(STDERR_FILENO, "ERR %s\n", message);
    send_protocol_error(message);
    return -1;
}

static int fail_path(const char *message, const char *path)
{
    dprintf(STDERR_FILENO, "ERR %s: %s\n", message, path);
    send_protocol_error(message);
    return -1;
}

static int protocol_line(const char *line)
{
    return write_all(STDOUT_FILENO, line, strlen(line));
}

static int read_line(int fd, char *line, size_t capacity, bool *at_eof)
{
    size_t length = 0U;
    *at_eof = false;
    for (;;) {
        unsigned char byte = 0U;
        ssize_t count = read(fd, &byte, 1U);
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count == 0) {
            *at_eof = true;
            if (length == 0U) {
                return 0;
            }
            return -1;
        }
        if (count < 0) {
            return -1;
        }
        if (byte == '\n') {
            line[length] = '\0';
            return 0;
        }
        if (byte < 0x20U || byte > 0x7eU || length + 1U >= capacity) {
            return -1;
        }
        line[length++] = (char)byte;
    }
}

static int read_exact(int fd, void *data, size_t length)
{
    unsigned char *cursor = data;
    while (length > 0U) {
        ssize_t count = read(fd, cursor, length);
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count <= 0) {
            return -1;
        }
        cursor += (size_t)count;
        length -= (size_t)count;
    }
    return 0;
}

static bool valid_room_id(const char *room_id)
{
    if (strlen(room_id) != ROOM_ID_LEN) {
        return false;
    }
    for (size_t index = 0U; index < ROOM_ID_LEN; index++) {
        unsigned char byte = (unsigned char)room_id[index];
        if (!islower(byte) && !isdigit(byte)) {
            return false;
        }
    }
    return true;
}

static int parse_decimal(const char *text, uint64_t maximum, uint64_t *value)
{
    if (*text == '\0') {
        return -1;
    }
    uint64_t parsed = 0U;
    for (const unsigned char *cursor = (const unsigned char *)text;
         *cursor != '\0'; cursor++) {
        if (!isdigit(*cursor)) {
            return -1;
        }
        unsigned int digit = (unsigned int)(*cursor - (unsigned char)'0');
        if (parsed > (maximum - digit) / 10U) {
            return -1;
        }
        parsed = parsed * 10U + digit;
    }
    *value = parsed;
    return 0;
}

static int parse_pair(char *line, const char *expected, char **value)
{
    char *space = strchr(line, ' ');
    if (space == NULL || strchr(space + 1, ' ') != NULL) {
        return -1;
    }
    *space = '\0';
    if (strcmp(line, expected) != 0 || space[1] == '\0') {
        return -1;
    }
    *value = space + 1;
    return 0;
}

static int parse_header(char *line, const char *expected, size_t maximum,
                        size_t *length)
{
    char *value = NULL;
    uint64_t parsed = 0U;
    if (parse_pair(line, expected, &value) < 0 ||
        parse_decimal(value, (uint64_t)maximum, &parsed) < 0) {
        return -1;
    }
    *length = (size_t)parsed;
    return 0;
}

static int parse_entropy_frame(struct payload *payload)
{
    char line[MAX_LINE_LEN];
    bool at_eof = false;
    size_t length = 0U;

    if (read_line(STDIN_FILENO, line, sizeof(line), &at_eof) < 0 || at_eof ||
        parse_header(line, "ENTROPY", ENTROPY_LEN, &length) < 0 ||
        length != ENTROPY_LEN ||
        read_exact(STDIN_FILENO, payload->entropy, ENTROPY_LEN) < 0) {
        return fail("invalid ENTROPY frame");
    }
    return 0;
}

static int parse_payload(struct payload *payload)
{
    char line[MAX_LINE_LEN];
    char *value = NULL;
    bool at_eof = false;
    uint64_t epoch = 0U;
    size_t length = 0U;

    if (read_line(STDIN_FILENO, line, sizeof(line), &at_eof) < 0 || at_eof ||
        parse_pair(line, "IDENTITY", &value) < 0 || !valid_room_id(value)) {
        return fail("invalid IDENTITY line");
    }
    memcpy(payload->room_id, value, ROOM_ID_LEN + 1U);

    if (read_line(STDIN_FILENO, line, sizeof(line), &at_eof) < 0 || at_eof ||
        parse_pair(line, "CLOCK", &value) < 0 ||
        parse_decimal(value, (uint64_t)INT64_MAX, &epoch) < 0 || epoch == 0U) {
        return fail("invalid CLOCK line");
    }
    payload->epoch = (time_t)epoch;
    if ((uint64_t)payload->epoch != epoch) {
        return fail("CLOCK epoch is outside time_t range");
    }

    if (read_line(STDIN_FILENO, line, sizeof(line), &at_eof) < 0 || at_eof ||
        parse_header(line, "SECRETS", MAX_SECRETS_LEN, &length) < 0) {
        return fail("invalid SECRETS frame");
    }
    payload->secrets_len = length;
    if (length > 0U) {
        payload->secrets = malloc(length);
        if (payload->secrets == NULL ||
            read_exact(STDIN_FILENO, payload->secrets, length) < 0) {
            return fail("short SECRETS frame");
        }
    }

    if (read_line(STDIN_FILENO, line, sizeof(line), &at_eof) < 0 || at_eof ||
        parse_header(line, "END", 0U, &length) < 0 || length != 0U) {
        return fail("malformed END frame");
    }
    return 0;
}

static int path_set(char *destination, const char *root, const char *suffix)
{
    int count = snprintf(destination, PATH_MAX, "%s%s", root, suffix);
    return count > 0 && count < PATH_MAX ? 0 : -1;
}

static int init_paths(struct paths *paths, const char *root)
{
#define SET_PATH(field, suffix) \
    if (path_set(paths->field, root, suffix) < 0) return fail("path is too long")
    SET_PATH(run_rooms, "/run/rooms");
    SET_PATH(identity, "/run/rooms/identity");
    SET_PATH(secrets_env, "/run/rooms/secrets.env");
    SET_PATH(rooms_home, "/home/rooms");
    SET_PATH(global_git, "/home/rooms/.gitconfig");
    SET_PATH(workspace, "/workspace");
    SET_PATH(repo, "/workspace/repo");
    SET_PATH(repo_git, "/workspace/repo/.git");
    SET_PATH(repo_config, "/workspace/repo/.git/config");
    SET_PATH(repo_worktree_config, "/workspace/repo/.git/config.worktree");
    SET_PATH(repo_identity, "/workspace/repo/.git/rooms-identity");
    SET_PATH(ssh_dir, "/etc/ssh");
    SET_PATH(ssh_key, "/etc/ssh/ssh_host_ed25519_key");
    SET_PATH(ssh_pub, "/etc/ssh/ssh_host_ed25519_key.pub");
    SET_PATH(sshd_config, "/etc/ssh/sshd_config.rooms-resume");
    SET_PATH(sshd_runtime, "/run/sshd");
    SET_PATH(sshd_pid, "/run/sshd.pid");
    SET_PATH(sudoers_dir, "/etc/sudoers.d");
    SET_PATH(sudoers_file, "/etc/sudoers.d/rooms");
    SET_PATH(sudoers, "/etc/sudoers");
#undef SET_PATH
    return 0;
}

static int connect_resume_stream(void)
{
#ifndef __linux__
    return fail("retained resume transport requires Linux");
#else
    const struct sockaddr_vm address = {
        .svm_family = AF_VSOCK,
        .svm_port = RESUME_PORT,
        .svm_cid = RESUME_HOST_CID,
    };
    const struct timeval io_timeout = {
        .tv_sec = RESUME_IO_TIMEOUT_SECONDS,
        .tv_usec = 0,
    };
    const struct timespec retry = {
        .tv_sec = 0,
        .tv_nsec = RESUME_CONNECT_MS * 1000000L,
    };

    for (;;) {
        int fd = socket(AF_VSOCK, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
        bool connected = false;
        if (fd >= 0) {
            if (connect(fd, (const struct sockaddr *)&address, sizeof(address)) == 0) {
                connected = true;
            } else if (errno == EINPROGRESS) {
                struct pollfd event = {.fd = fd, .events = POLLOUT, .revents = 0};
                int ready;
                do {
                    ready = poll(&event, 1U, RESUME_CONNECT_MS);
                } while (ready < 0 && errno == EINTR);
                if (ready > 0) {
                    int socket_error = 0;
                    socklen_t length = sizeof(socket_error);
                    connected = getsockopt(fd, SOL_SOCKET, SO_ERROR,
                                           &socket_error, &length) == 0 &&
                                socket_error == 0;
                }
            }
        }

        if (connected) {
            int flags = fcntl(fd, F_GETFL);
            if (flags >= 0 && fcntl(fd, F_SETFL, flags & ~O_NONBLOCK) == 0 &&
                setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO,
                           &io_timeout, sizeof(io_timeout)) == 0 &&
                setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO,
                           &io_timeout, sizeof(io_timeout)) == 0) {
                return fd;
            }
        }
        if (fd >= 0) close(fd);

        struct timespec remaining = retry;
        while (nanosleep(&remaining, &remaining) < 0 && errno == EINTR) {
        }
    }
#endif
}

static int attach_resume_stream(void)
{
    int fd = connect_resume_stream();
    if (fd < 0) return -1;
    if (dup2(fd, STDIN_FILENO) < 0 || dup2(fd, STDOUT_FILENO) < 0) {
        close(fd);
        return fail("cannot attach retained resume transport");
    }
    if (fd > STDOUT_FILENO) close(fd);
    resume_stream_attached = true;
    return 0;
}

static bool mode_has_unsafe_write(mode_t mode)
{
    return (mode & (S_IWGRP | S_IWOTH)) != 0;
}

static int require_directory(const char *path, uid_t uid, gid_t gid,
                             mode_t exact_mode)
{
    struct stat status;
    if (lstat(path, &status) < 0 || !S_ISDIR(status.st_mode) ||
        status.st_uid != uid || status.st_gid != gid ||
        (status.st_mode & 07777) != exact_mode) {
        return fail_path("unsafe directory", path);
    }
    return 0;
}

static int require_protected_directory(const char *path, uid_t uid, gid_t gid)
{
    struct stat status;
    if (lstat(path, &status) < 0 || !S_ISDIR(status.st_mode) ||
        status.st_uid != uid || status.st_gid != gid ||
        mode_has_unsafe_write(status.st_mode)) {
        return fail_path("unsafe protected directory", path);
    }
    return 0;
}

static int ensure_directory(const char *path, uid_t uid, gid_t gid, mode_t mode)
{
    struct stat status;
    if (lstat(path, &status) < 0) {
        if (errno != ENOENT || mkdir(path, mode) < 0 || chown(path, uid, gid) < 0 ||
            chmod(path, mode) < 0) {
            return fail_path("cannot create directory", path);
        }
    }
    return require_directory(path, uid, gid, mode);
}

static int destination_is_safe(const char *path, uid_t uid, gid_t gid)
{
    struct stat status;
    if (lstat(path, &status) < 0) {
        return errno == ENOENT ? 0 : fail_path("cannot inspect destination", path);
    }
    if (!S_ISREG(status.st_mode) || status.st_uid != uid || status.st_gid != gid ||
        mode_has_unsafe_write(status.st_mode)) {
        return fail_path("unsafe destination", path);
    }
    return 0;
}

static int parent_path(const char *path, char *parent)
{
    size_t length = strlen(path);
    if (length == 0U || length >= PATH_MAX) {
        return -1;
    }
    memcpy(parent, path, length + 1U);
    char *slash = strrchr(parent, '/');
    if (slash == NULL || slash == parent) {
        return -1;
    }
    *slash = '\0';
    return 0;
}

static int fsync_parent(const char *path)
{
    char parent[PATH_MAX];
    if (parent_path(path, parent) < 0) {
        return -1;
    }
    int fd = open(parent, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) {
        return -1;
    }
    int result = fsync(fd);
    int saved = errno;
    close(fd);
    errno = saved;
    return result;
}

static int atomic_write(const char *path, const void *data, size_t length,
                        uid_t uid, gid_t gid, mode_t mode)
{
    char pending[PATH_MAX];
    int count = snprintf(pending, sizeof(pending), "%s.rooms-new", path);
    if (count <= 0 || (size_t)count >= sizeof(pending) ||
        destination_is_safe(path, uid, gid) < 0) {
        return -1;
    }
    struct stat status;
    if (lstat(pending, &status) == 0 || errno != ENOENT) {
        return fail_path("pending path already exists", pending);
    }
    int fd = open(pending, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW,
                  mode);
    if (fd < 0) {
        return fail_path("cannot create pending file", pending);
    }
    int result = 0;
    if (fchown(fd, uid, gid) < 0 || fchmod(fd, mode) < 0 ||
        write_all(fd, data, length) < 0 || fsync(fd) < 0) {
        result = fail_path("cannot write pending file", pending);
    }
    if (close(fd) < 0 && result == 0) {
        result = fail_path("cannot close pending file", pending);
    }
    if (result == 0 && destination_is_safe(path, uid, gid) < 0) {
        result = -1;
    }
    if (result == 0 && rename(pending, path) < 0) {
        result = fail_path("cannot install file", path);
    }
    if (result == 0 && fsync_parent(path) < 0) {
        result = fail_path("cannot sync parent directory", path);
    }
    if (result < 0) {
        unlink(pending);
    }
    return result;
}

static int read_regular_file(const char *path, uid_t uid, gid_t gid,
                             unsigned char **bytes, size_t *length)
{
    struct stat status;
    if (lstat(path, &status) < 0 || !S_ISREG(status.st_mode) ||
        status.st_uid != uid || status.st_gid != gid ||
        mode_has_unsafe_write(status.st_mode) || status.st_size < 0 ||
        (uint64_t)status.st_size > MAX_CONFIG_LEN) {
        return fail_path("unsafe regular file", path);
    }
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) {
        return fail_path("cannot open regular file", path);
    }
    size_t size = (size_t)status.st_size;
    unsigned char *content = malloc(size + 1U);
    if (content == NULL || read_exact(fd, content, size) < 0) {
        close(fd);
        free(content);
        return fail_path("cannot read regular file", path);
    }
    close(fd);
    content[size] = '\0';
    if (memchr(content, '\0', size) != NULL) {
        free(content);
        return fail_path("regular file contains NUL", path);
    }
    *bytes = content;
    *length = size;
    return 0;
}

static int reseed(const unsigned char entropy[ENTROPY_LEN])
{
#ifndef __linux__
    (void)entropy;
    return fail("kernel CRNG reseed requires Linux");
#else
    int fd = open("/dev/urandom", O_WRONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0 || write_all(fd, entropy, ENTROPY_LEN) < 0) {
        if (fd >= 0) close(fd);
        return fail("cannot mix restore entropy into /dev/urandom");
    }
    /* A write only mixes the input pool.  An already-ready CRNG can otherwise
     * keep serving the cloned per-CPU stream until its periodic reseed.  Force
     * a generation change before any child process can draw randomness. */
    if (ioctl(fd, RNDRESEEDCRNG, 0) < 0) {
        close(fd);
        return fail("cannot force the kernel CRNG reseed");
    }
    if (close(fd) < 0) {
        return fail("cannot close /dev/urandom");
    }
    return 0;
#endif
}

static int step_clock(time_t epoch)
{
    struct timespec value = {.tv_sec = epoch, .tv_nsec = 0};
    if (clock_settime(CLOCK_REALTIME, &value) < 0) {
        return fail("cannot step clock");
    }
    return 0;
}

static int stage_identity(const struct paths *paths, const char *room_id,
                          uid_t owner_uid, gid_t owner_gid)
{
    char value[ROOM_ID_LEN + 2U];
    int count = snprintf(value, sizeof(value), "%s\n", room_id);
    if (count != (int)ROOM_ID_LEN + 1 ||
        atomic_write(paths->identity, value, (size_t)count,
                     owner_uid, owner_gid, 0644) < 0) {
        return fail("cannot stage room identity");
    }
    return 0;
}

static int remove_regular_or_absent(const char *path, uid_t uid, gid_t gid)
{
    struct stat status;
    if (lstat(path, &status) < 0) {
        return errno == ENOENT ? 0 : fail_path("cannot inspect optional file", path);
    }
    if (!S_ISREG(status.st_mode) || status.st_uid != uid || status.st_gid != gid) {
        return fail_path("refusing to remove unsafe file", path);
    }
    return unlink(path) == 0 ? 0 : fail_path("cannot remove file", path);
}

static int stage_secrets(const struct paths *paths, const struct payload *payload,
                         uid_t rooms_uid, gid_t rooms_gid)
{
    if (payload->secrets_len == 0U) {
        return remove_regular_or_absent(paths->secrets_env, rooms_uid, rooms_gid);
    }
    if (atomic_write(paths->secrets_env, payload->secrets, payload->secrets_len,
                     rooms_uid, rooms_gid, 0600) < 0) {
        return fail("cannot stage resume secrets");
    }
    return 0;
}

static int write_global_identity(const struct paths *paths, const char *room_id,
                                 uid_t rooms_uid, gid_t rooms_gid)
{
    char content[160];
    int count = snprintf(content, sizeof(content),
                         "[user]\n\tname = rooms %s\n\temail = %s@rooms.invalid\n",
                         room_id, room_id);
    if (count <= 0 || (size_t)count >= sizeof(content) ||
        atomic_write(paths->global_git, content, (size_t)count, rooms_uid,
                     rooms_gid, 0600) < 0) {
        return fail("cannot install global git identity");
    }
    return 0;
}

static int read_repo_config(const char *path, bool allow_absent,
                            uid_t rooms_uid, gid_t rooms_gid,
                            unsigned char **config, size_t *config_len)
{
    struct stat status;
    if (lstat(path, &status) == 0) {
        return read_regular_file(path, rooms_uid, rooms_gid, config, config_len);
    }
    if (errno != ENOENT || !allow_absent) {
        return fail_path("cannot inspect repository config", path);
    }
    *config = malloc(1U);
    if (*config == NULL) return fail("cannot allocate empty repository config");
    *config_len = 0U;
    return 0;
}

static int append_repo_identity_include(const char *path, bool allow_absent,
                                        uid_t rooms_uid, gid_t rooms_gid)
{
    unsigned char *config = NULL;
    size_t config_len = 0U;
    if (read_repo_config(path, allow_absent, rooms_uid, rooms_gid,
                         &config, &config_len) < 0) {
        return -1;
    }
    if (memmem(config, config_len, "rooms-identity", strlen("rooms-identity")) != NULL) {
        free(config);
        return fail_path("repository config already names the resume identity include", path);
    }

    size_t include_len = strlen(repo_include);
    if (config_len > SIZE_MAX - include_len) {
        free(config);
        return fail("repository config is too large");
    }
    unsigned char *updated = malloc(config_len + include_len);
    if (updated == NULL) {
        free(config);
        return fail("cannot allocate repository config");
    }
    memcpy(updated, config, config_len);
    memcpy(updated + config_len, repo_include, include_len);
    free(config);
    int result = atomic_write(path, updated, config_len + include_len,
                              rooms_uid, rooms_gid, 0600);
    scrub(updated, config_len + include_len);
    free(updated);
    return result;
}

static int write_repo_identity(const struct paths *paths, const char *room_id,
                               uid_t rooms_uid, gid_t rooms_gid)
{
    struct stat status;
    if (lstat(paths->repo, &status) < 0 && errno == ENOENT) {
        return 0;
    }
    if (require_protected_directory(paths->workspace, rooms_uid, rooms_gid) < 0 ||
        require_protected_directory(paths->repo, rooms_uid, rooms_gid) < 0 ||
        require_protected_directory(paths->repo_git, rooms_uid, rooms_gid) < 0) {
        return -1;
    }

    char identity[160];
    int identity_len = snprintf(identity, sizeof(identity),
                                "[user]\n\tname = rooms %s\n\temail = %s@rooms.invalid\n",
                                room_id, room_id);
    if (identity_len <= 0 || (size_t)identity_len >= sizeof(identity) ||
        atomic_write(paths->repo_identity, identity, (size_t)identity_len,
                     rooms_uid, rooms_gid, 0600) < 0) {
        return fail("cannot install repository identity include");
    }
    if (append_repo_identity_include(paths->repo_config, false,
                                     rooms_uid, rooms_gid) < 0 ||
        append_repo_identity_include(paths->repo_worktree_config, true,
                                     rooms_uid, rooms_gid) < 0) {
        return fail("cannot install repository git identity");
    }
    return 0;
}

static int install_git_identity(const struct paths *paths, const char *room_id,
                                uid_t rooms_uid, gid_t rooms_gid)
{
    if (require_protected_directory(paths->rooms_home, rooms_uid, rooms_gid) < 0 ||
        write_global_identity(paths, room_id, rooms_uid, rooms_gid) < 0 ||
        write_repo_identity(paths, room_id, rooms_uid, rooms_gid) < 0) {
        return -1;
    }
    return 0;
}

static int wait_command(char *const arguments[], int output_fd)
{
    pid_t child = fork();
    if (child < 0) {
        return -1;
    }
    if (child == 0) {
        int null_fd = open("/dev/null", O_RDWR | O_CLOEXEC);
        if (null_fd < 0) _exit(126);
        if (dup2(null_fd, STDIN_FILENO) < 0 ||
            dup2(output_fd >= 0 ? output_fd : null_fd, STDOUT_FILENO) < 0 ||
            dup2(null_fd, STDERR_FILENO) < 0) {
            _exit(126);
        }
        if (null_fd > STDERR_FILENO) close(null_fd);
        execv(arguments[0], arguments);
        _exit(127);
    }
    int status = 0;
    while (waitpid(child, &status, 0) < 0) {
        if (errno != EINTR) return -1;
    }
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
}

static int wait_command_as_user(char *const arguments[], uid_t uid, gid_t gid)
{
    pid_t child = fork();
    if (child < 0) return -1;
    if (child == 0) {
        int null_fd = open("/dev/null", O_RDWR | O_CLOEXEC);
        if (null_fd < 0 || dup2(null_fd, STDIN_FILENO) < 0 ||
            dup2(null_fd, STDOUT_FILENO) < 0 || dup2(null_fd, STDERR_FILENO) < 0 ||
            setgroups(0U, NULL) < 0 || setgid(gid) < 0 || setuid(uid) < 0) {
            _exit(126);
        }
        if (null_fd > STDERR_FILENO) close(null_fd);
        char *environment[] = {
            "HOME=/home/rooms",
            "USER=rooms",
            "LOGNAME=rooms",
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "LC_ALL=C",
            NULL,
        };
        execve(arguments[0], arguments, environment);
        _exit(127);
    }
    int status = 0;
    while (waitpid(child, &status, 0) < 0) {
        if (errno != EINTR) return -1;
    }
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
}

static int clear_host_keys(const struct paths *paths)
{
    if (require_protected_directory(paths->ssh_dir, 0U, 0U) < 0) {
        return -1;
    }
    DIR *directory = opendir(paths->ssh_dir);
    if (directory == NULL) {
        return fail("cannot open SSH configuration directory");
    }
    int directory_fd = dirfd(directory);
    if (directory_fd < 0) {
        closedir(directory);
        return fail("cannot resolve SSH configuration directory descriptor");
    }
    int result = 0;
    struct dirent *entry = NULL;
    errno = 0;
    while ((entry = readdir(directory)) != NULL) {
        if (strncmp(entry->d_name, "ssh_host_", strlen("ssh_host_")) != 0) {
            continue;
        }
        struct stat status;
        if (fstatat(directory_fd, entry->d_name, &status, AT_SYMLINK_NOFOLLOW) < 0 ||
            S_ISDIR(status.st_mode) || unlinkat(directory_fd, entry->d_name, 0) < 0) {
            result = fail("cannot clear SSH host-key path");
            break;
        }
    }
    if (result == 0 && errno != 0) result = fail("cannot enumerate SSH host-key paths");
    closedir(directory);
    return result;
}

static int require_regular_exact(const char *path, uid_t uid, gid_t gid,
                                 mode_t mode, bool nonempty)
{
    struct stat status;
    if (lstat(path, &status) < 0 || !S_ISREG(status.st_mode) ||
        status.st_uid != uid || status.st_gid != gid ||
        (status.st_mode & 07777) != mode || (nonempty && status.st_size <= 0)) {
        return fail_path("file has unsafe type, ownership, mode, or size", path);
    }
    return 0;
}

static int regular_file_equals(const char *path, uid_t uid, gid_t gid,
                               mode_t mode, const void *expected, size_t length)
{
    if (require_regular_exact(path, uid, gid, mode, length > 0U) < 0) return -1;
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    unsigned char *content = malloc(length + 1U);
    if (content == NULL) {
        close(fd);
        return -1;
    }
    unsigned char trailing = 0U;
    int result = 0;
    if (read_exact(fd, content, length) < 0 ||
        read(fd, &trailing, 1U) != 0 || memcmp(content, expected, length) != 0) {
        result = -1;
    }
    scrub(content, length + 1U);
    free(content);
    close(fd);
    return result;
}

static int host_key_set_is_exact(const struct paths *paths)
{
    DIR *directory = opendir(paths->ssh_dir);
    if (directory == NULL) return -1;
    unsigned int count = 0U;
    bool private_seen = false;
    bool public_seen = false;
    struct dirent *entry = NULL;
    errno = 0;
    while ((entry = readdir(directory)) != NULL) {
        if (strncmp(entry->d_name, "ssh_host_", strlen("ssh_host_")) != 0) continue;
        count++;
        if (strcmp(entry->d_name, "ssh_host_ed25519_key") == 0) private_seen = true;
        if (strcmp(entry->d_name, "ssh_host_ed25519_key.pub") == 0) public_seen = true;
    }
    int read_error = errno;
    closedir(directory);
    return read_error == 0 && count == 2U && private_seen && public_seen ? 0 : -1;
}

static int generate_host_key(const struct paths *paths)
{
    if (clear_host_keys(paths) < 0) return -1;
    char *arguments[] = {
        "/usr/bin/ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f",
        (char *)paths->ssh_key, NULL,
    };
    mode_t previous_umask = umask(0077);
    int generated = wait_command(arguments, -1);
    umask(previous_umask);
    if (generated < 0 || chmod(paths->ssh_key, 0600) < 0 ||
        chmod(paths->ssh_pub, 0644) < 0 ||
        require_regular_exact(paths->ssh_key, 0U, 0U, 0600, true) < 0 ||
        require_regular_exact(paths->ssh_pub, 0U, 0U, 0644, true) < 0 ||
        host_key_set_is_exact(paths) < 0) {
        return fail("fresh Ed25519 SSH host-key generation failed");
    }
    return 0;
}

static char *skip_space(char *cursor)
{
    while (*cursor == ' ' || *cursor == '\t') cursor++;
    return cursor;
}

static bool parse_directive(char *line, char **name, char **value)
{
    char *cursor = skip_space(line);
    if (*cursor == '\0' || *cursor == '#') return false;
    *name = cursor;
    while (*cursor != '\0' && *cursor != '=' && !isspace((unsigned char)*cursor)) cursor++;
    if (*cursor == '\0') {
        *value = cursor;
        return true;
    }
    *cursor++ = '\0';
    cursor = skip_space(cursor);
    if (*cursor == '=') cursor = skip_space(cursor + 1);
    *value = cursor;
    char *end = cursor + strlen(cursor);
    while (end > cursor && isspace((unsigned char)end[-1])) *--end = '\0';
    return true;
}

static int canonical_sshd_config_is_safe(const struct paths *paths,
                                         uid_t owner_uid, gid_t owner_gid)
{
    return regular_file_equals(paths->sshd_config, owner_uid, owner_gid, 0644,
                               canonical_sshd_config,
                               strlen(canonical_sshd_config)) < 0
               ? fail("canonical sshd config differs from the sealed rooms policy")
               : 0;
}

struct sshd_expectation {
    const char *name;
    const char *value;
    unsigned int count;
};

static int record_sshd_expectation(struct sshd_expectation *expectations,
                                   size_t count, const char *name,
                                   const char *value)
{
    for (size_t index = 0U; index < count; index++) {
        if (strcasecmp(expectations[index].name, name) != 0) continue;
        expectations[index].count++;
        return strcmp(expectations[index].value, value) == 0 ? 0 : -1;
    }
    return 0;
}

static bool sshd_expectations_are_exact(const struct sshd_expectation *expectations,
                                        size_t count)
{
    for (size_t index = 0U; index < count; index++) {
        if (expectations[index].count != 1U) return false;
    }
    return true;
}

static int effective_sshd_config_is_safe(const struct paths *paths)
{
    struct sshd_expectation expectations[] = {
        {"port", "22", 0U},
        {"addressfamily", "inet", 0U},
        {"listenaddress", "0.0.0.0:22", 0U},
        {"hostkey", "/etc/ssh/ssh_host_ed25519_key", 0U},
        {"pidfile", "/run/sshd.pid", 0U},
        {"permitrootlogin", "no", 0U},
        {"pubkeyauthentication", "yes", 0U},
        {"authenticationmethods", "publickey", 0U},
        {"passwordauthentication", "no", 0U},
        {"kbdinteractiveauthentication", "no", 0U},
        {"permitemptypasswords", "no", 0U},
        {"usedns", "no", 0U},
        {"allowusers", "rooms", 0U},
        {"authorizedkeysfile", ".ssh/authorized_keys", 0U},
        {"strictmodes", "yes", 0U},
        {"allowagentforwarding", "no", 0U},
        {"allowtcpforwarding", "no", 0U},
        {"x11forwarding", "no", 0U},
        {"permittunnel", "no", 0U},
        {"permituserenvironment", "no", 0U},
    };
    const size_t expectation_count = sizeof(expectations) / sizeof(expectations[0]);
    int output[2];
    if (pipe(output) < 0) return fail("cannot create sshd validation pipe");
    pid_t child = fork();
    if (child < 0) {
        close(output[0]); close(output[1]);
        return fail("cannot fork sshd validation");
    }
    if (child == 0) {
        int null_fd = open("/dev/null", O_RDWR | O_CLOEXEC);
        if (null_fd < 0 || dup2(null_fd, STDIN_FILENO) < 0 ||
            dup2(output[1], STDOUT_FILENO) < 0 || dup2(null_fd, STDERR_FILENO) < 0) {
            _exit(126);
        }
        close(output[0]); close(output[1]);
        execl("/usr/sbin/sshd", "/usr/sbin/sshd", "-T", "-f", paths->sshd_config,
              "-C", "user=rooms,host=rooms-agent,addr=127.0.0.1", (char *)NULL);
        _exit(127);
    }
    close(output[1]);
    char line[MAX_LINE_LEN];
    bool at_eof = false;
    int result = 0;
    while (!at_eof) {
        if (read_line(output[0], line, sizeof(line), &at_eof) < 0) {
            result = fail("invalid sshd effective-config output");
            break;
        }
        if (at_eof) break;
        char *name = NULL;
        char *value = NULL;
        if (!parse_directive(line, &name, &value)) continue;
        if (record_sshd_expectation(expectations, expectation_count,
                                    name, value) < 0) result = -1;
    }
    close(output[0]);
    int status = 0;
    while (waitpid(child, &status, 0) < 0) {
        if (errno != EINTR) { result = -1; break; }
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 ||
        !sshd_expectations_are_exact(expectations, expectation_count)) result = -1;
    return result < 0 ? fail("effective sshd config differs from the sealed rooms policy") : 0;
}

static int restore_sudo(const struct paths *paths, uid_t rooms_uid, gid_t rooms_gid)
{
    if (require_protected_directory(paths->sudoers_dir, 0U, 0U) < 0 ||
        require_regular_exact(paths->sudoers, 0U, 0U, 0440, true) < 0 ||
        atomic_write(paths->sudoers_file, sudo_grant, strlen(sudo_grant),
                     0U, 0U, 0440) < 0 ||
        regular_file_equals(paths->sudoers_file, 0U, 0U, 0440,
                            sudo_grant, strlen(sudo_grant)) < 0) {
        return fail("cannot install workload sudo grant");
    }
    char *arguments[] = {"/usr/sbin/visudo", "-cf", (char *)paths->sudoers, NULL};
    if (wait_command(arguments, -1) < 0) {
        unlink(paths->sudoers_file);
        return fail("effective sudo policy failed validation");
    }
    char *probe[] = {"/usr/bin/sudo", "-n", "/bin/true", NULL};
    if (wait_command_as_user(probe, rooms_uid, rooms_gid) < 0) {
        unlink(paths->sudoers_file);
        return fail("workload sudo grant is not effective as rooms");
    }
    return require_regular_exact(paths->sudoers_file, 0U, 0U, 0440, true);
}

static bool tcp22_is_listening_in(const char *table)
{
    FILE *file = fopen(table, "re");
    if (file == NULL) return false;
    char *line = NULL;
    size_t capacity = 0U;
    bool found = false;
    while (getline(&line, &capacity, file) >= 0) {
        char local[80];
        char remote[80];
        unsigned int slot = 0U;
        unsigned int state = 0U;
        if (sscanf(line, " %u: %79s %79s %x", &slot, local, remote, &state) != 4) continue;
        char *colon = strrchr(local, ':');
        if (colon != NULL && strcasecmp(colon + 1, "0016") == 0 && state == 0x0aU) {
            found = true;
            break;
        }
    }
    free(line);
    fclose(file);
    return found;
}

static bool tcp22_is_listening(void)
{
    return tcp22_is_listening_in("/proc/net/tcp") ||
           tcp22_is_listening_in("/proc/net/tcp6");
}

static int read_sshd_pid(const struct paths *paths, pid_t *pid)
{
    int fd = open(paths->sshd_pid,
                  O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK);
    if (fd < 0) return -1;

    struct stat status;
    if (fstat(fd, &status) < 0 || !S_ISREG(status.st_mode) ||
        status.st_uid != 0U || status.st_gid != 0U ||
        mode_has_unsafe_write(status.st_mode) || status.st_size <= 0 ||
        status.st_size > 31) {
        close(fd);
        return -1;
    }
    size_t length = (size_t)status.st_size;
    unsigned char content[32];
    unsigned char trailing = 0U;
    int read_result = read_exact(fd, content, length);
    ssize_t trailing_result = -1;
    if (read_result == 0) {
        do {
            trailing_result = read(fd, &trailing, 1U);
        } while (trailing_result < 0 && errno == EINTR);
    }
    int close_result = close(fd);
    if (read_result < 0 || trailing_result != 0 || close_result < 0) return -1;
    content[length] = '\0';
    if (content[length - 1U] == '\n') content[--length] = '\0';
    uint64_t parsed = 0U;
    int result = parse_decimal((char *)content, (uint64_t)INT_MAX, &parsed);
    if (result < 0 || parsed <= 1U) return -1;
    *pid = (pid_t)parsed;
    return 0;
}

static bool process_is_sshd(pid_t pid)
{
    char process_exe[64];
    int count = snprintf(process_exe, sizeof(process_exe), "/proc/%ld/exe", (long)pid);
    if (count <= 0 || (size_t)count >= sizeof(process_exe)) return false;
    struct stat expected;
    struct stat actual;
    return kill(pid, 0) == 0 && stat("/usr/sbin/sshd", &expected) == 0 &&
           stat(process_exe, &actual) == 0 && expected.st_dev == actual.st_dev &&
           expected.st_ino == actual.st_ino;
}

static bool exact_ipv4_listener_inode(unsigned long *inode)
{
    FILE *file = fopen("/proc/net/tcp", "re");
    if (file == NULL) return false;
    char *line = NULL;
    size_t capacity = 0U;
    unsigned int matches = 0U;
    unsigned long matched_inode = 0U;
    while (getline(&line, &capacity, file) >= 0) {
        char *tokens[10] = {0};
        size_t token_count = 0U;
        char *save = NULL;
        for (char *token = strtok_r(line, " \t\n", &save);
             token != NULL && token_count < 10U;
             token = strtok_r(NULL, " \t\n", &save)) {
            tokens[token_count++] = token;
        }
        if (token_count < 10U || strcmp(tokens[1], "00000000:0016") != 0 ||
            strcmp(tokens[2], "00000000:0000") != 0 ||
            strcasecmp(tokens[3], "0A") != 0) {
            continue;
        }
        errno = 0;
        char *end = NULL;
        unsigned long parsed = strtoul(tokens[9], &end, 10);
        if (errno != 0 || end == tokens[9] || *end != '\0' || parsed == 0U) continue;
        matches++;
        matched_inode = parsed;
    }
    free(line);
    fclose(file);
    if (matches != 1U) return false;
    *inode = matched_inode;
    return true;
}

static bool process_owns_socket(pid_t pid, unsigned long inode)
{
    char fd_path[64];
    int count = snprintf(fd_path, sizeof(fd_path), "/proc/%ld/fd", (long)pid);
    if (count <= 0 || (size_t)count >= sizeof(fd_path)) return false;
    DIR *directory = opendir(fd_path);
    if (directory == NULL) return false;
    char expected[64];
    count = snprintf(expected, sizeof(expected), "socket:[%lu]", inode);
    if (count <= 0 || (size_t)count >= sizeof(expected)) {
        closedir(directory);
        return false;
    }
    bool owned = false;
    int directory_fd = dirfd(directory);
    struct dirent *entry = NULL;
    while (directory_fd >= 0 && (entry = readdir(directory)) != NULL) {
        char target[64];
        ssize_t length = readlinkat(directory_fd, entry->d_name,
                                    target, sizeof(target) - 1U);
        if (length < 0 || (size_t)length >= sizeof(target)) continue;
        target[length] = '\0';
        if (strcmp(target, expected) == 0) {
            owned = true;
            break;
        }
    }
    closedir(directory);
    return owned;
}

static bool sshd_owns_reachable_listener(const struct paths *paths)
{
    pid_t pid = 0;
    unsigned long inode = 0U;
    return read_sshd_pid(paths, &pid) == 0 && process_is_sshd(pid) &&
           exact_ipv4_listener_inode(&inode) && process_owns_socket(pid, inode);
}

static int launch_sshd(const struct paths *paths)
{
    if (tcp22_is_listening()) return fail("SSH port is already listening");
    if (ensure_directory(paths->sshd_runtime, 0U, 0U, 0755) < 0 ||
        remove_regular_or_absent(paths->sshd_pid, 0U, 0U) < 0) {
        return -1;
    }
    char *arguments[] = {"/usr/sbin/sshd", "-f", (char *)paths->sshd_config, NULL};
    if (wait_command(arguments, -1) < 0) return fail("cannot launch sshd");
    struct timespec pause = {.tv_sec = 0, .tv_nsec = LISTEN_WAIT_NS};
    for (unsigned int attempt = 0U; attempt < LISTEN_WAIT_TRIES; attempt++) {
        if (sshd_owns_reachable_listener(paths)) return 0;
        nanosleep(&pause, NULL);
    }
    return fail("launched sshd did not own the reachable port 22 listener");
}

static int prepare_control_paths(const struct paths *paths,
                                 uid_t rooms_uid, gid_t rooms_gid)
{
    /* Sticky root:rooms ownership lets rooms delete its own one-shot secret
     * while preventing it from replacing the root-owned identity record. */
    if (ensure_directory(paths->run_rooms, 0U, rooms_gid, 01770) < 0 ||
        require_protected_directory(paths->rooms_home, rooms_uid, rooms_gid) < 0 ||
        require_protected_directory(paths->ssh_dir, 0U, 0U) < 0) {
        return -1;
    }
    return 0;
}

static int apply_payload(const struct paths *paths, struct payload *payload,
                         uid_t rooms_uid, gid_t rooms_gid)
{
    set_protocol_stage("clock");
    if (step_clock(payload->epoch) < 0 || protocol_line("STEP clock\n") < 0) return -1;
    set_protocol_stage("identity");
    if (prepare_control_paths(paths, rooms_uid, rooms_gid) < 0) return -1;
    if (stage_identity(paths, payload->room_id, 0U, 0U) < 0 ||
        stage_secrets(paths, payload, rooms_uid, rooms_gid) < 0 ||
        install_git_identity(paths, payload->room_id, rooms_uid, rooms_gid) < 0 ||
        protocol_line("STEP identity\n") < 0) return -1;
    scrub(payload->secrets, payload->secrets_len);

    set_protocol_stage("hostkeys");
    if (generate_host_key(paths) < 0 ||
        canonical_sshd_config_is_safe(paths, 0U, 0U) < 0 ||
        ensure_directory(paths->sshd_runtime, 0U, 0U, 0755) < 0 ||
        effective_sshd_config_is_safe(paths) < 0 ||
        protocol_line("STEP hostkeys\n") < 0) return -1;

    set_protocol_stage("privilege");
    if (restore_sudo(paths, rooms_uid, rooms_gid) < 0 ||
        protocol_line("STEP privilege\n") < 0) return -1;
    set_protocol_stage("sshd");
    if (launch_sshd(paths) < 0 || protocol_line("STEP sshd\n") < 0) return -1;
    set_protocol_stage("complete");
    return protocol_line("ACK resume\n");
}

static int resume_session(const struct paths *paths)
{
    set_protocol_stage("entropy");
    if (protocol_line("ROOMS-RESUME/1\n") < 0 ||
        parse_entropy_frame(&sensitive_payload) < 0) {
        return -1;
    }
    set_protocol_stage("reseed");
    if (reseed(sensitive_payload.entropy) < 0) return -1;
    scrub(sensitive_payload.entropy, sizeof(sensitive_payload.entropy));
    if (protocol_line("STEP reseeded\n") < 0) return -1;

    /* Defer even libc account lookup until after the forced reseed. The
     * retained pre-snapshot steady state is only socket/poll/nanosleep and
     * owns no allocator-initialized random cookie or userspace DRBG. */
    set_protocol_stage("payload");
    struct passwd *rooms = getpwnam("rooms");
    if (rooms == NULL) return fail("cannot resolve rooms user");
    if (parse_payload(&sensitive_payload) < 0) return -1;
    return apply_payload(paths, &sensitive_payload, rooms->pw_uid, rooms->pw_gid);
}

static int prewarm(const struct paths *paths)
{
    if (canonical_sshd_config_is_safe(paths, 0U, 0U) < 0) return -1;
    int fd = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    if (fd < 0) return fail("cannot open resume helper for prewarm");
    unsigned char buffer[16384];
    int result = 0;
    for (;;) {
        ssize_t count = read(fd, buffer, sizeof(buffer));
        if (count > 0) continue;
        if (count == 0) break;
        if (errno == EINTR) continue;
        result = fail("cannot read resume helper for prewarm");
        break;
    }
    close(fd);
    scrub(buffer, sizeof(buffer));
    return result;
}

#ifdef ROOMS_RESUME_TEST
static int test_parse_only(void)
{
    if (protocol_line("ROOMS-RESUME/1\n") < 0 ||
        parse_entropy_frame(&sensitive_payload) < 0 ||
        parse_payload(&sensitive_payload) < 0) return 1;
    dprintf(STDOUT_FILENO, "PARSED %s %lld %zu\n", sensitive_payload.room_id,
            (long long)sensitive_payload.epoch, sensitive_payload.secrets_len);
    return 0;
}

static int test_stage(const char *root, const char *room_id)
{
    struct paths paths;
    if (!valid_room_id(room_id) || init_paths(&paths, root) < 0) return 1;
    struct stat home;
    if (lstat(paths.rooms_home, &home) < 0 || !S_ISDIR(home.st_mode)) return 1;
    uid_t uid = home.st_uid;
    gid_t gid = home.st_gid;
    sensitive_payload.secrets = (unsigned char *)strdup("TOKEN=test\n");
    sensitive_payload.secrets_len = strlen((char *)sensitive_payload.secrets);
    if (ensure_directory(paths.run_rooms, uid, gid, 01770) < 0 ||
        stage_identity(&paths, room_id, uid, gid) < 0 ||
        stage_secrets(&paths, &sensitive_payload, uid, gid) < 0 ||
        install_git_identity(&paths, room_id, uid, gid) < 0) return 1;
    return 0;
}

static int test_config(const char *root)
{
    struct paths paths;
    struct stat status;
    if (init_paths(&paths, root) < 0 || lstat(paths.sshd_config, &status) < 0) return 1;
    return canonical_sshd_config_is_safe(&paths, status.st_uid, status.st_gid) < 0 ? 1 : 0;
}

static int test_protocol_error(void)
{
    resume_stream_attached = true;
    set_protocol_stage("hostkeys");
    return fail("this deliberately overlong diagnostic is truncated at the protocol boundary");
}

static int test_missing_sshd_pid(const char *root, bool terminal)
{
    struct paths paths;
    pid_t pid = 0;
    if (init_paths(&paths, root) < 0) return 1;
    resume_stream_attached = true;
    set_protocol_stage("sshd");
    if (read_sshd_pid(&paths, &pid) == 0) return 1;
    if (!terminal) return 0;
    (void)fail("launched sshd did not own the reachable port 22 listener");
    return 1;
}
#endif

int main(int argc, char **argv)
{
    if (atexit(scrub_payload) != 0) return 1;
    struct paths paths;
    if (init_paths(&paths, "") < 0) return 1;

#ifdef ROOMS_RESUME_TEST
    if (argc == 2 && strcmp(argv[1], "--test-parse") == 0) return test_parse_only();
    if (argc == 2 && strcmp(argv[1], "--test-error") == 0) return test_protocol_error();
    if (argc == 3 && strcmp(argv[1], "--test-sshd-pid-absent") == 0) {
        return test_missing_sshd_pid(argv[2], false);
    }
    if (argc == 3 && strcmp(argv[1], "--test-sshd-terminal") == 0) {
        return test_missing_sshd_pid(argv[2], true);
    }
    if (argc == 4 && strcmp(argv[1], "--test-stage") == 0) return test_stage(argv[2], argv[3]);
    if (argc == 3 && strcmp(argv[1], "--test-config") == 0) return test_config(argv[2]);
#endif

    if (argc == 2 && strcmp(argv[1], "--prewarm") == 0) return prewarm(&paths) < 0 ? 1 : 0;
    if (argc != 2 || strcmp(argv[1], "--loop") != 0) {
        return fail("usage: rooms-resume-apply --loop|--prewarm") < 0 ? 1 : 1;
    }
    if (geteuid() != 0U) return fail("rooms-resume-apply must run as root") < 0 ? 1 : 1;

    if (attach_resume_stream() < 0) return 1;
    return resume_session(&paths) < 0 ? 1 : 0;
}
