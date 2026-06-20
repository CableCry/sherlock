// crash_uaf.c — builds a heap data structure, then crashes a few frames deep.
//
//   sherlock testing/crash_uaf.c
//
// Exercises the Phase 5 crash handler: the program allocates a linked list,
// frees it, and then dereferences a NULL pointer deep in the call stack. The
// profiler catches the SIGSEGV, captures the faulting backtrace, and writes a
// final CRASH event — the dashboard shows the red CRASH panel (signal, faulting
// address, PC, and the backtrace) before the process core-dumps naturally.
#include <stdlib.h>

struct node {
    struct node *next;
    int value;
};

static struct node *build(int n) {
    struct node *head = NULL;
    for (int i = 0; i < n; i++) {
        struct node *m = malloc(sizeof *m);
        if (!m) {
            exit(EXIT_FAILURE);
        }
        m->value = i;
        m->next = head;
        head = m;
    }
    return head;
}

static void free_list(struct node *head) {
    while (head) {
        struct node *next = head->next;
        free(head);
        head = next;
    }
}

static int total_after_free(struct node *head) {
    free_list(head);
    // Bug: walk a deliberately NULL'd pointer -> guaranteed SIGSEGV here.
    struct node *dangling = NULL;
    return dangling->value;
}

int main(void) {
    struct node *list = build(32);
    return total_after_free(list); // crashes a couple of frames deep
}
