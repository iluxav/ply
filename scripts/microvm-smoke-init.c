/* PID 1 for the microVM kernel's boot smoke test.
 *
 * Why this exists
 * ---------------
 * The build script asserts `CONFIG_X=y` for ~50 options. That check can only
 * ever contain what somebody already thought of, and `=y` proves PRESENCE,
 * not USABILITY: a parent can be `=y` with the child that makes it work
 * turned off. Two Critical defects shipped past a 39-assertion config check —
 * a kernel with no futex/epoll/shmget (so no PostgreSQL, redis, node or Go)
 * and a kernel whose overlay returned -EIO for `mkdir` — and both would have
 * been caught by ONE BOOT that tried the operations a real guest performs.
 *
 * So this program boots under qemu-system-aarch64 on the Image the build just
 * produced and exercises the syscalls and mounts that ply's guests actually
 * use, one named check at a time. It is deliberately NOT ply-guest-init: it
 * must be buildable and runnable before that crate exists, must not drift
 * with it, and must depend on nothing but a static libc.
 *
 * Contract with scripts/microvm-smoke.sh:
 *   PASS <name>            gating check succeeded
 *   FAIL <name>: <why>     gating check failed  -> the build fails
 *   WARN <name>: <why>     reported, never gates (see hvc1 below)
 *   SMOKE-RESULT pass=N fail=N warn=N
 *   SMOKE-DONE             absence of this line means panic/hang, also a fail
 *
 * Build: gcc -static -O2 -o smoke-init microvm-smoke-init.c
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <linux/futex.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/inotify.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/random.h>
#include <sys/reboot.h>
#include <sys/shm.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/timerfd.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <sys/xattr.h>

static int n_pass, n_fail, n_warn;

static void say(const char *fmt, ...)
{
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    if (n > 0)
        (void)!write(1, buf, (size_t)n < sizeof buf ? (size_t)n : sizeof buf - 1);
}

static void pass(const char *name)
{
    n_pass++;
    say("PASS %s\n", name);
}

static void fail(const char *name, const char *fmt, ...)
{
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    n_fail++;
    say("FAIL %s: %s\n", name, buf);
}

static void warn(const char *name, const char *fmt, ...)
{
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    n_warn++;
    say("WARN %s: %s\n", name, buf);
}

/* errno -> "ENOSYS" where it matters, so a config gap reads as one. */
static const char *why(void)
{
    switch (errno) {
    case ENOSYS: return "ENOSYS (the syscall is not compiled in)";
    case ENODEV: return "ENODEV (no such filesystem type in this kernel)";
    case EIO:    return "EIO";
    case EXDEV:  return "EXDEV (invalid cross-device link)";
    case EOPNOTSUPP: return "EOPNOTSUPP (the filesystem does not support it)";
    default:     return strerror(errno);
    }
}

/* --- the checks ------------------------------------------------------- */

static void check_dev_node(const char *name, const char *path, unsigned maj, unsigned min)
{
    struct stat st;
    if (stat(path, &st) != 0) {
        fail(name, "stat(%s): %s", path, why());
        return;
    }
    if (!S_ISCHR(st.st_mode)) {
        fail(name, "%s is not a character device (mode %07o)", path, st.st_mode);
        return;
    }
    unsigned gm = major(st.st_rdev), gn = minor(st.st_rdev);
    if (gm != maj || gn != min) {
        fail(name, "%s is %u:%u, want %u:%u", path, gm, gn, maj, min);
        return;
    }
    pass(name);
}

static void check_epoll(void)
{
    int ep = epoll_create1(EPOLL_CLOEXEC);
    if (ep < 0) {
        fail("epoll_create1", "%s -- postgres WaitEventSet, redis, nginx and "
                             "node/libuv are all epoll", why());
        return;
    }
    int ev = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    if (ev < 0) {
        close(ep);
        fail("epoll_create1", "needed an eventfd to drive it: %s", why());
        return;
    }
    struct epoll_event e = {.events = EPOLLIN, .data.fd = ev};
    if (epoll_ctl(ep, EPOLL_CTL_ADD, ev, &e) != 0) {
        fail("epoll_create1", "epoll_ctl: %s", why());
        goto out;
    }
    uint64_t one = 1;
    if (write(ev, &one, sizeof one) != sizeof one) {
        fail("epoll_create1", "write(eventfd): %s", why());
        goto out;
    }
    struct epoll_event got;
    int n = epoll_wait(ep, &got, 1, 2000);
    if (n != 1) {
        fail("epoll_create1", "epoll_wait returned %d (want 1): %s", n, why());
        goto out;
    }
    pass("epoll_create1");
out:
    close(ev);
    close(ep);
}

static void check_eventfd(void)
{
    int fd = eventfd(7, EFD_CLOEXEC);
    if (fd < 0) {
        fail("eventfd", "%s -- libuv, tokio and glibc AIO wake their loops with it", why());
        return;
    }
    uint64_t v = 0;
    if (read(fd, &v, sizeof v) != sizeof v || v != 7)
        fail("eventfd", "read back %llu, want 7", (unsigned long long)v);
    else
        pass("eventfd");
    close(fd);
}

/* A genuinely contended futex: the child blocks in FUTEX_WAIT, the parent
 * wakes it. A bare "does the syscall exist" probe would miss a kernel where
 * the wait path is broken, which is the half that deadlocks a pthread. */
static void check_futex(void)
{
    int *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                  MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) {
        fail("futex", "mmap: %s", why());
        return;
    }
    m[0] = 1; /* the futex word the child waits on */
    m[1] = 0; /* child-is-about-to-wait flag */
    pid_t pid = fork();
    if (pid < 0) {
        fail("futex", "fork: %s", why());
        munmap(m, 4096);
        return;
    }
    if (pid == 0) {
        m[1] = 1;
        long r = syscall(SYS_futex, &m[0], FUTEX_WAIT, 1, NULL, NULL, 0);
        _exit(r == 0 ? 0 : (errno == ENOSYS ? 42 : 43));
    }
    /* Retry the wake: the child may not have reached FUTEX_WAIT yet, and a
     * wake with no waiter returns 0 rather than an error. */
    struct timespec ms = {0, 5 * 1000 * 1000};
    long woke = 0;
    for (int i = 0; i < 400 && woke <= 0; i++) {
        nanosleep(&ms, NULL);
        woke = syscall(SYS_futex, &m[0], FUTEX_WAKE, 1, NULL, NULL, 0);
        if (woke < 0) {
            fail("futex", "FUTEX_WAKE: %s -- every contended pthread mutex, "
                          "Rust std::sync and the Go scheduler need this", why());
            kill(pid, SIGKILL);
            waitpid(pid, NULL, 0);
            munmap(m, 4096);
            return;
        }
    }
    int st = 0;
    waitpid(pid, &st, 0);
    if (woke != 1)
        fail("futex", "FUTEX_WAKE woke %ld waiters, want 1", woke);
    else if (!WIFEXITED(st) || WEXITSTATUS(st) != 0)
        fail("futex", "the waiting child exited %d (42 = FUTEX_WAIT ENOSYS)",
             WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    else
        pass("futex");
    munmap(m, 4096);
}

static void check_shmget(void)
{
    int id = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600);
    if (id < 0) {
        fail("shmget", "%s -- postgres creates a System V segment on EVERY "
                       "start as its postmaster interlock, even with "
                       "shared_memory_type=mmap", why());
        return;
    }
    char *p = shmat(id, NULL, 0);
    if (p == (char *)-1) {
        fail("shmget", "shmat: %s", why());
        shmctl(id, IPC_RMID, NULL);
        return;
    }
    memcpy(p, "ply", 4);
    int bad = strcmp(p, "ply") != 0;
    shmdt(p);
    shmctl(id, IPC_RMID, NULL);
    if (bad)
        fail("shmget", "the segment did not hold what was written to it");
    else
        pass("shmget");
}

static void check_flock(void)
{
    const char *path = "/smoke-lock";
    int a = open(path, O_RDWR | O_CREAT | O_CLOEXEC, 0600);
    if (a < 0) {
        fail("flock", "open: %s", why());
        return;
    }
    if (flock(a, LOCK_EX | LOCK_NB) != 0) {
        fail("flock", "%s -- sqlite, dpkg, cargo and postgres.pid all lock", why());
        close(a);
        return;
    }
    /* A second open file description must be refused, or the lock is a no-op. */
    int b = open(path, O_RDWR | O_CLOEXEC);
    int refused = (b >= 0 && flock(b, LOCK_EX | LOCK_NB) != 0
                   && (errno == EWOULDBLOCK || errno == EAGAIN));
    if (b >= 0)
        close(b);
    if (!refused) {
        fail("flock", "a second flock(LOCK_EX|LOCK_NB) was GRANTED -- the lock "
                      "does not actually exclude");
        close(a);
        return;
    }
    struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
    if (fcntl(a, F_SETLK, &fl) != 0) {
        fail("flock", "fcntl(F_SETLK): %s", why());
        close(a);
        return;
    }
    close(a);
    unlink(path);
    pass("flock");
}

static void check_inotify(void)
{
    if (mkdir("/smoke-ino", 0755) != 0 && errno != EEXIST) {
        fail("inotify_init1", "mkdir: %s", why());
        return;
    }
    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    if (fd < 0) {
        fail("inotify_init1", "%s -- Task 6 watches /run/ply/self with inotify "
                              "to forward the app's published keys; without it "
                              "`publish` silently never fires", why());
        return;
    }
    int wd = inotify_add_watch(fd, "/smoke-ino", IN_CREATE | IN_CLOSE_WRITE);
    if (wd < 0) {
        fail("inotify_init1", "inotify_add_watch: %s", why());
        close(fd);
        return;
    }
    int t = open("/smoke-ino/key", O_WRONLY | O_CREAT | O_CLOEXEC, 0644);
    if (t >= 0) {
        (void)!write(t, "v", 1);
        close(t);
    }
    struct pollfd pfd = {.fd = fd, .events = POLLIN};
    if (poll(&pfd, 1, 2000) != 1) {
        fail("inotify_init1", "no event arrived within 2s for a file that was "
                              "just created in the watched directory");
        close(fd);
        return;
    }
    char buf[4096];
    ssize_t n = read(fd, buf, sizeof buf);
    close(fd);
    unlink("/smoke-ino/key");
    rmdir("/smoke-ino");
    if (n <= 0)
        fail("inotify_init1", "poll said readable but read returned %zd", n);
    else
        pass("inotify_init1");
}

static void check_timerfd(void)
{
    int fd = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC);
    if (fd < 0) {
        fail("timerfd_create", "%s -- libuv's uv_timer and most Go/Rust timer "
                               "wheels are timerfds", why());
        return;
    }
    struct itimerspec its = {.it_value = {0, 20 * 1000 * 1000}};
    if (timerfd_settime(fd, 0, &its, NULL) != 0) {
        fail("timerfd_create", "timerfd_settime: %s", why());
        close(fd);
        return;
    }
    struct pollfd pfd = {.fd = fd, .events = POLLIN};
    if (poll(&pfd, 1, 5000) != 1) {
        fail("timerfd_create", "a 20ms timer did not fire within 5s");
        close(fd);
        return;
    }
    uint64_t ticks = 0;
    ssize_t n = read(fd, &ticks, sizeof ticks);
    close(fd);
    if (n != sizeof ticks || ticks == 0)
        fail("timerfd_create", "read %zd bytes, %llu expirations", n,
             (unsigned long long)ticks);
    else
        pass("timerfd_create");
}

static void check_signalfd(void)
{
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &mask, NULL) != 0) {
        fail("signalfd", "sigprocmask: %s", why());
        return;
    }
    int fd = signalfd(-1, &mask, SFD_CLOEXEC);
    if (fd < 0) {
        fail("signalfd", "%s -- the standard way a server turns SIGTERM into a "
                         "pollable fd; ply sends SIGTERM on `ply down`", why());
        sigprocmask(SIG_UNBLOCK, &mask, NULL);
        return;
    }
    raise(SIGUSR1);
    struct signalfd_siginfo si;
    ssize_t n = read(fd, &si, sizeof si);
    close(fd);
    sigprocmask(SIG_UNBLOCK, &mask, NULL);
    if (n != sizeof si)
        fail("signalfd", "read %zd bytes, want %zu", n, sizeof si);
    else if (si.ssi_signo != (uint32_t)SIGUSR1)
        fail("signalfd", "delivered signal %u, want %d", si.ssi_signo, SIGUSR1);
    else
        pass("signalfd");
}

static void check_posix_timers(void)
{
    timer_t tid;
    if (timer_create(CLOCK_MONOTONIC, NULL, &tid) != 0) {
        fail("posix_timers", "timer_create: %s -- glibc nanosleep/sleep(3), "
                             "pthread_cond_timedwait, the JVM and .NET", why());
        return;
    }
    timer_delete(tid);
    pass("posix_timers");
}

static void check_advise_syscalls(void)
{
    int fd = open("/smoke-advise", O_RDWR | O_CREAT | O_CLOEXEC, 0600);
    if (fd < 0) {
        fail("advise_syscalls", "open: %s", why());
        return;
    }
    (void)!write(fd, "0123456789", 10);
    long r = syscall(SYS_fadvise64, fd, (off_t)0, (off_t)0, POSIX_FADV_DONTNEED);
    close(fd);
    unlink("/smoke-advise");
    if (r != 0 && errno == ENOSYS) {
        fail("advise_syscalls", "fadvise64: %s -- glibc malloc, jemalloc and "
                                "Go's heap release lean on madvise", why());
        return;
    }
    void *p = mmap(NULL, 65536, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        fail("advise_syscalls", "mmap: %s", why());
        return;
    }
    int mr = madvise(p, 65536, MADV_DONTNEED);
    munmap(p, 65536);
    if (mr != 0)
        fail("advise_syscalls", "madvise(MADV_DONTNEED): %s", why());
    else
        pass("advise_syscalls");
}

static void check_rseq(void)
{
    /* NULL/0 is never a valid registration: EINVAL means the syscall is here,
     * ENOSYS means it is not. glibc >= 2.35 registers rseq for every thread. */
    errno = 0;
    long r = syscall(SYS_rseq, (void *)0, 0u, 0, 0u);
    if (r < 0 && errno == ENOSYS)
        fail("rseq", "%s -- glibc registers rseq for every thread at startup", why());
    else
        pass("rseq");
}

static void check_membarrier(void)
{
    errno = 0;
    long r = syscall(SYS_membarrier, 0 /* MEMBARRIER_CMD_QUERY */, 0, 0);
    if (r < 0)
        fail("membarrier", "MEMBARRIER_CMD_QUERY: %s -- .NET GC suspension, "
                           "ZGC/Shenandoah handshakes, userspace RCU", why());
    else
        pass("membarrier");
}

static void check_aio(void)
{
    unsigned long ctx = 0;
    errno = 0;
    if (syscall(SYS_io_setup, 1, &ctx) != 0) {
        fail("aio", "io_setup: %s -- MariaDB's innodb_use_native_aio is on by "
                    "default", why());
        return;
    }
    syscall(SYS_io_destroy, ctx);
    pass("aio");
}

static void check_io_uring(void)
{
#ifdef SYS_io_uring_setup
    /* NULL params: EFAULT means the syscall exists, ENOSYS means it does not. */
    errno = 0;
    long r = syscall(SYS_io_uring_setup, 1, (void *)0);
    if (r < 0 && errno == ENOSYS)
        fail("io_uring", "%s -- PostgreSQL 18 io_method=io_uring and tokio-uring "
                         "probe for it", why());
    else
        pass("io_uring");
#else
    warn("io_uring", "this libc's headers have no SYS_io_uring_setup");
#endif
}

static void check_fhandle(void)
{
    struct {
        struct file_handle h;
        unsigned char pad[128];
    } fh;
    memset(&fh, 0, sizeof fh);
    fh.h.handle_bytes = sizeof fh.pad;
    int mnt = 0;
    errno = 0;
    if (name_to_handle_at(AT_FDCWD, "/init", &fh.h, &mnt, 0) != 0 && errno == ENOSYS)
        fail("fhandle", "name_to_handle_at: %s", why());
    else
        pass("fhandle");
}

static void check_seccomp(void)
{
    /* PR_GET_SECCOMP has no side effect and returns EINVAL when CONFIG_SECCOMP
     * is off. PR_SET_SECCOMP would kill this process, which is not a test. */
    errno = 0;
    int r = prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
    if (r < 0)
        fail("seccomp", "prctl(PR_GET_SECCOMP): %s -- sshd's privsep child and "
                        "Chromium/Electron call fatal() when seccomp is refused",
             why());
    else
        pass("seccomp");
}

/* Report-only: the guest may legitimately have no entropy source yet, and the
 * VMM (Task 8) is what decides whether a virtio-rng device exists at all. */
static void report_entropy(void)
{
    unsigned char b[16];
    errno = 0;
    ssize_t n = getrandom(b, sizeof b, GRND_NONBLOCK);
    int hw = access("/dev/hwrng", F_OK) == 0;
    if (n == (ssize_t)sizeof b)
        say("PASS entropy (getrandom non-blocking at boot; /dev/hwrng %s)\n",
            hw ? "present" : "absent"), n_pass++;
    else
        warn("entropy", "getrandom(GRND_NONBLOCK) -> %zd (%s); /dev/hwrng %s. "
                        "Any app that reads /dev/random early will STALL until "
                        "the CRNG seeds -- Task 8 should attach a virtio-rng",
             n, strerror(errno), hw ? "present" : "absent");
}

static void check_consoles(void)
{
    int fd = open("/dev/hvc0", O_WRONLY | O_CLOEXEC);
    if (fd < 0) {
        fail("hvc0", "open(/dev/hvc0): %s -- this is where the app's stdout "
                     "goes", why());
    } else {
        const char *msg = "microvm smoke: hvc0 is writable\n";
        ssize_t w = write(fd, msg, strlen(msg));
        close(fd);
        if (w < 0)
            fail("hvc0", "write: %s", why());
        else
            pass("hvc0");
    }

    /* NEVER a failure. CONFIG_VIRTIO_CONSOLE gives /dev/hvc0; a further
     * multiport port only becomes /dev/hvcN if the DEVICE sends
     * VIRTIO_CONSOLE_CONSOLE_PORT for it, otherwise it is /dev/vport0p<n>.
     * The kernel config cannot guarantee hvc1 -- Task 8's device model must. */
    int have1 = access("/dev/hvc1", F_OK) == 0;
    int havevp = access("/dev/vport0p1", F_OK) == 0;
    say("INFO hvc1 %s, /dev/vport0p1 %s -- reported, not gated: whether the "
        "control port shows up as hvc1 is the VMM's job (Task 8), not the "
        "kernel config's\n",
        have1 ? "present" : "absent", havevp ? "present" : "absent");
}

/* --- squashfs + overlay: the C2 defect, exactly ------------------------ */

static void check_layers(const char *layer_dev)
{
    if (mkdir("/lower", 0755) != 0 && errno != EEXIST) {
        fail("squashfs_mount", "mkdir /lower: %s", why());
        return;
    }
    if (mount(layer_dev, "/lower", "squashfs", MS_RDONLY, NULL) != 0) {
        fail("squashfs_mount", "mount(%s, squashfs): %s -- image layers are "
                               "squashfs; nothing runs without this", layer_dev, why());
        return;
    }
    pass("squashfs_mount");

    /* The layer is zstd-compressed, which is what `ply build` writes; reading
     * a byte out of it is the only proof CONFIG_SQUASHFS_ZSTD is real. */
    int fd = open("/lower/hello", O_RDONLY | O_CLOEXEC);
    char buf[64] = {0};
    ssize_t n = fd >= 0 ? read(fd, buf, sizeof buf - 1) : -1;
    if (fd >= 0)
        close(fd);
    if (n <= 0 || strncmp(buf, "hello from the layer", 20) != 0)
        fail("squashfs_zstd_read", "read /lower/hello -> %zd %.20s", n, buf);
    else
        pass("squashfs_zstd_read");

    char x[64] = {0};
    ssize_t xn = getxattr("/lower/xattrfile", "user.plysmoke", x, sizeof x - 1);
    if (xn < 0)
        fail("squashfs_xattr", "getxattr(user.plysmoke) on a layer file: %s -- "
                               "without CONFIG_SQUASHFS_XATTR, file capabilities "
                               "and user.* xattrs baked into an image layer are "
                               "SILENTLY dropped in the guest", why());
    else if (strcmp(x, "ok") != 0)
        fail("squashfs_xattr", "xattr read back as \"%s\"", x);
    else
        pass("squashfs_xattr");

    if (mkdir("/upper", 0755) != 0 && errno != EEXIST) {
        fail("tmpfs_mount", "mkdir /upper: %s", why());
        return;
    }
    if (mount("tmpfs", "/upper", "tmpfs", 0, "mode=0755") != 0) {
        fail("tmpfs_mount", "mount tmpfs: %s", why());
        return;
    }
    pass("tmpfs_mount");

    /* Direct probe of the exact thing overlayfs probes at mount time. */
    if (setxattr("/upper", "trusted.plysmoke", "ok", 2, 0) != 0)
        fail("tmpfs_xattr", "setxattr(trusted.*) on the tmpfs upper: %s -- "
                            "overlayfs sets ofs->noxattr and then returns EIO "
                            "from ovl_set_opaque()", why());
    else
        pass("tmpfs_xattr");

    if ((mkdir("/upper/u", 0755) != 0 && errno != EEXIST)
        || (mkdir("/upper/w", 0755) != 0 && errno != EEXIST)
        || (mkdir("/ovl", 0755) != 0 && errno != EEXIST)) {
        fail("overlay_mount", "mkdir: %s", why());
        return;
    }
    if (mount("overlay", "/ovl", "overlay", 0,
              "lowerdir=/lower,upperdir=/upper/u,workdir=/upper/w") != 0) {
        fail("overlay_mount", "mount overlay: %s", why());
        return;
    }
    pass("overlay_mount");

    /* Copy-up of a plain file: the ordinary case, so a failure below is
     * specific to opaque directories rather than to overlays in general. */
    int w = open("/ovl/hello", O_WRONLY | O_APPEND | O_CLOEXEC);
    if (w < 0 || write(w, "!", 1) != 1)
        fail("overlay_copy_up", "append to a lower file: %s", why());
    else
        pass("overlay_copy_up");
    if (w >= 0)
        close(w);

    /* ===== the C2 defect =====
     * A directory that exists in an image layer is removed and recreated --
     * `rm -rf /var/lib/foo && mkdir /var/lib/foo`, which every postinst,
     * every "reset the data dir", every test fixture does. The new directory
     * must be marked opaque to hide the lower one, and ovl_set_opaque()
     * returns -EIO when the upper filesystem has no xattrs. */
    if (unlink("/ovl/optest/keep") != 0 && errno != ENOENT) {
        fail("overlay_mkdir_over_lower", "unlink /ovl/optest/keep: %s", why());
    } else if (rmdir("/ovl/optest") != 0) {
        fail("overlay_mkdir_over_lower", "rmdir a directory from the lower "
                                         "layer: %s", why());
    } else if (mkdir("/ovl/optest", 0755) != 0) {
        fail("overlay_mkdir_over_lower",
             "mkdir over a directory that exists in the lower layer: %s -- this "
             "is `rm -rf /var/lib/foo && mkdir /var/lib/foo` inside every ply "
             "microVM, and EIO here means CONFIG_TMPFS_XATTR is off", why());
    } else {
        struct stat st;
        if (stat("/ovl/optest/keep", &st) == 0)
            fail("overlay_mkdir_over_lower",
                 "the recreated directory is not opaque: the lower layer's "
                 "'keep' is still visible inside it");
        else
            pass("overlay_mkdir_over_lower");
    }

    /* ===== the second half of C2: renaming a lower directory ===== */
    if (rename("/ovl/renameme", "/ovl/renamed") != 0) {
        fail("overlay_rename_lower_dir",
             "rename a directory that came from the lower layer: %s -- EXDEV "
             "here means CONFIG_OVERLAY_FS_REDIRECT_DIR is off (or xattrs are, "
             "which silently downgrades it to redirect_dir=nofollow)", why());
    } else {
        int f = open("/ovl/renamed/f", O_RDONLY | O_CLOEXEC);
        if (f < 0)
            fail("overlay_rename_lower_dir",
                 "the renamed directory lost its contents: %s", why());
        else {
            close(f);
            pass("overlay_rename_lower_dir");
        }
    }
}

/* --- ext4 volume: mke2fs, mount, write, read back --------------------- */

static void check_ext4(const char *dev)
{
    pid_t pid = fork();
    if (pid < 0) {
        fail("mke2fs", "fork: %s", why());
        return;
    }
    if (pid == 0) {
        /* stderr stays on the console on purpose: when mke2fs refuses, its
         * one line of output is the whole diagnosis. */
        execl("/sbin/mke2fs", "mke2fs", "-q", "-F", "-t", "ext4", dev, (char *)NULL);
        _exit(127);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
        fail("mke2fs", "/sbin/mke2fs %s exited %d (127 = not executable in the "
                       "initramfs)", dev, WIFEXITED(st) ? WEXITSTATUS(st) : -1);
        return;
    }
    pass("mke2fs");

    if (mkdir("/vol", 0755) != 0 && errno != EEXIST) {
        fail("ext4_mount", "mkdir /vol: %s", why());
        return;
    }
    if (mount(dev, "/vol", "ext4", 0, NULL) != 0) {
        fail("ext4_mount", "mount(%s, ext4): %s -- the spike saw exactly this "
                           "when CONFIG_EXT4_FS was missing: mkfs succeeds, the "
                           "mount fails with ENODEV", dev, why());
        return;
    }
    pass("ext4_mount");

    int fd = open("/vol/data", O_WRONLY | O_CREAT | O_CLOEXEC, 0644);
    if (fd < 0 || write(fd, "volume payload", 14) != 14 || fsync(fd) != 0) {
        fail("ext4_write_read", "write: %s", why());
        if (fd >= 0)
            close(fd);
        umount("/vol");
        return;
    }
    close(fd);
    /* Round-trip through umount/mount, so this proves the bytes reached the
     * device and not just the page cache. */
    if (umount("/vol") != 0) {
        fail("ext4_write_read", "umount: %s", why());
        return;
    }
    if (mount(dev, "/vol", "ext4", 0, NULL) != 0) {
        fail("ext4_write_read", "remount: %s", why());
        return;
    }
    char buf[32] = {0};
    fd = open("/vol/data", O_RDONLY | O_CLOEXEC);
    ssize_t n = fd >= 0 ? read(fd, buf, sizeof buf - 1) : -1;
    if (fd >= 0)
        close(fd);
    umount("/vol");
    if (n != 14 || strcmp(buf, "volume payload") != 0)
        fail("ext4_write_read", "read back %zd bytes: %.20s", n, buf);
    else
        pass("ext4_write_read");
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    say("SMOKE-BEGIN\n");

    struct utsname u;
    if (uname(&u) == 0)
        say("INFO kernel %s %s %s\n", u.sysname, u.release, u.machine);

    /* /dev/console and /dev/null as the INITRAMFS carries them, before
     * devtmpfs is mounted over the top -- ruling R0-4 is about the archive. */
    check_dev_node("dev_console_initramfs", "/dev/console", 5, 1);
    check_dev_node("dev_null_initramfs", "/dev/null", 1, 3);

    if (mkdir("/proc", 0755) != 0 && errno != EEXIST)
        say("INFO mkdir /proc: %s\n", strerror(errno));
    if (mount("proc", "/proc", "proc", 0, NULL) != 0)
        fail("mount_proc", "%s", why());
    else
        pass("mount_proc");
    if (mkdir("/sys", 0755) != 0 && errno != EEXIST)
        say("INFO mkdir /sys: %s\n", strerror(errno));
    if (mount("sysfs", "/sys", "sysfs", 0, NULL) != 0)
        fail("mount_sysfs", "%s", why());
    else
        pass("mount_sysfs");

    /* CONFIG_DEVTMPFS_MOUNT is a documented no-op for initramfs boots, so the
     * init mounts /dev itself -- exactly as Task 6's guest init must. */
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) != 0)
        fail("mount_devtmpfs", "%s -- the guest init has to mount /dev itself; "
                               "CONFIG_DEVTMPFS_MOUNT does not do it for an "
                               "initramfs boot", why());
    else
        pass("mount_devtmpfs");

    check_dev_node("dev_console_devtmpfs", "/dev/console", 5, 1);
    check_dev_node("dev_null_devtmpfs", "/dev/null", 1, 3);

    check_consoles();

    check_epoll();
    check_eventfd();
    check_futex();
    check_shmget();
    check_flock();
    check_inotify();
    check_timerfd();
    check_signalfd();
    check_posix_timers();
    check_advise_syscalls();
    check_rseq();
    check_membarrier();
    check_aio();
    check_io_uring();
    check_fhandle();
    check_seccomp();
    report_entropy();

    /* Which /dev/vdN is which is NOT the order the disks were given to the
     * VMM: qemu's `virt` machine hands out virtio-mmio transports in reverse
     * command-line order, so `-drive layer` first came up as /dev/vdb. The
     * same class of surprise is why ruling R0-5 says to find the spec disk by
     * SCANNING for its magic rather than by trusting a position. Do the same
     * here: the layer is whichever disk mounts as squashfs. TASK 8: your DTB
     * decides this mapping -- state it, do not inherit it. */
    char devs[8][16];
    int ndev = 0;
    for (char c = 'a'; c <= 'h' && ndev < 8; c++) {
        snprintf(devs[ndev], sizeof devs[0], "/dev/vd%c", c);
        if (access(devs[ndev], F_OK) == 0)
            ndev++;
    }
    if (ndev < 2) {
        fail("virtio_blk", "found %d virtio disk(s), want 2 -- no virtio-mmio "
                           "transport or no virtio-blk driver means a guest "
                           "that boots and finds no disks", ndev);
    } else {
        say("INFO virtio disks: %d found (%s .. %s)\n", ndev, devs[0], devs[ndev - 1]);
        pass("virtio_blk");
        int layer = -1;
        if (mkdir("/lower", 0755) != 0 && errno != EEXIST)
            fail("squashfs_mount", "mkdir /lower: %s", why());
        for (int i = 0; i < ndev && layer < 0; i++)
            if (mount(devs[i], "/lower", "squashfs", MS_RDONLY, NULL) == 0) {
                layer = i;
                umount("/lower");
            }
        if (layer < 0) {
            fail("squashfs_mount", "none of the %d virtio disks mounts as "
                                   "squashfs -- image layers are squashfs, so "
                                   "nothing runs without this", ndev);
        } else {
            say("INFO layer disk is %s\n", devs[layer]);
            check_layers(devs[layer]);
        }
        int vol = layer == 0 ? 1 : 0;
        say("INFO volume disk is %s\n", devs[vol]);
        check_ext4(devs[vol]);
    }

    say("SMOKE-RESULT pass=%d fail=%d warn=%d\n", n_pass, n_fail, n_warn);
    say("SMOKE-DONE\n");

    sync();
    /* PSCI SYSTEM_OFF. Task 8's VMM must handle this HVC, or a guest can
     * never shut itself down. */
    reboot(RB_POWER_OFF);
    reboot(RB_AUTOBOOT);
    for (;;)
        pause();
}
