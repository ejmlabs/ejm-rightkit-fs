#if defined(__APPLE__)
#include <sys/acl.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int create_private(const char *path)
{
    acl_t acl = NULL;
    acl_flagset_t flags;
    filesec_t security = NULL;
    mode_t mode = 0600;
    int result = -1;

    acl = acl_init(0);
    if (acl == NULL || acl_get_flagset_np(acl, &flags) != 0 ||
        acl_add_flag_np(flags, ACL_FLAG_NO_INHERIT) != 0)
        goto out;
    security = filesec_init();
    if (security == NULL || filesec_set_property(security, FILESEC_MODE, &mode) != 0 ||
        filesec_set_property(security, FILESEC_ACL, &acl) != 0)
        goto out;
    result = openx_np(path, O_CREAT | O_EXCL | O_RDWR | O_NOFOLLOW | O_CLOEXEC, security);
out:
    if (security != NULL)
        filesec_free(security);
    if (acl != NULL)
        acl_free(acl);
    return result;
}

int main(int argc, char **argv)
{
    struct stat before, published;
    int fd, c;
    static const char header[] = "rightkit-lease-guard v2\n";

    if (argc != 4 || (strcmp(argv[1], "control") && strcmp(argv[1], "private"))) {
        fprintf(stderr, "usage: guard-open-probe control|private TEMP PUBLISHED\n");
        return 2;
    }
    umask(077);
    fd = !strcmp(argv[1], "control")
        ? open(argv[2], O_CREAT | O_EXCL | O_RDWR | O_NOFOLLOW | O_CLOEXEC, 0600)
        : create_private(argv[2]);
    if (fd < 0) { perror("create"); return 1; }
    if (fstat(fd, &before) != 0 || !S_ISREG(before.st_mode) || (before.st_mode & 077)) {
        fprintf(stderr, "created inode is not a private-mode regular file\n");
        return 1;
    }
    puts("created");
    fflush(stdout);
    c = getchar();
    if (c == EOF || write(fd, header, sizeof(header) - 1) != (ssize_t)(sizeof(header) - 1) ||
        flock(fd, LOCK_EX | LOCK_NB) != 0 || link(argv[2], argv[3]) != 0 ||
        lstat(argv[3], &published) != 0 || published.st_dev != before.st_dev ||
        published.st_ino != before.st_ino) {
        perror("initialize, lock or publish");
        return 1;
    }
    puts("published");
    fflush(stdout);
    if (getchar() == EOF) return 1;
    return close(fd) == 0 ? 0 : 1;
}
#else
#include <stdio.h>
int main(void) { fputs("macOS is required\n", stderr); return 77; }
#endif
