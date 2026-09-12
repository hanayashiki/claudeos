/* A plain C program linked against musl, to show that the kernel's Linux ABI
 * is not specific to Rust. No system headers are used, so this builds on a
 * host that has no Linux sysroot; the declarations come from musl itself. */

int printf(const char *format, ...);
int puts(const char *s);
long write(int fd, const void *buf, unsigned long count);
int getpid(void);
void *malloc(unsigned long size);
void free(void *p);
int atoi(const char *s);
unsigned long strlen(const char *s);

static unsigned long fib(unsigned long n)
{
    unsigned long a = 0, b = 1;
    for (unsigned long i = 0; i < n; i++) {
        unsigned long next = a + b;
        a = b;
        b = next;
    }
    return a;
}

int main(int argc, char **argv)
{
    puts("hello from C, linked against musl");
    printf("  pid           : %d\n", getpid());
    printf("  argc          : %d\n", argc);
    for (int i = 0; i < argc; i++)
        printf("  argv[%d]       : %s\n", i, argv[i]);

    unsigned long n = argc > 1 ? (unsigned long)atoi(argv[1]) : 40;
    printf("  fib(%lu)       : %lu\n", n, fib(n));

    unsigned long size = 1 << 20;
    char *block = malloc(size);
    if (!block) {
        puts("  malloc failed");
        return 1;
    }
    for (unsigned long i = 0; i < size; i += 4096)
        block[i] = (char)i;
    printf("  touched       : %lu KiB of heap\n", size / 1024);
    free(block);

    const char *message = "  direct write  : bypassing stdio\n";
    write(1, message, strlen(message));
    return 0;
}
