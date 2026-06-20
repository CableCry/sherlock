// cpp_workload.cpp — C++ allocations to exercise symbol demangling and
// multi-library resolution.
//
//   sherlock testing/cpp_workload.cpp
//
// std::string / std::vector / new all route through the interposed allocator,
// and their frames land in libstdc++ and libc as well as this binary — so the
// offenders/flame/call-stack views show demangled C++ names (e.g.
// `Widget::Widget(int)`) and resolved library frames. One cache is leaked on
// purpose, so the Leaks view (4) flags it as a definite leak once you quit and
// the process is torn down. Runs until you press q.
#include <chrono>
#include <string>
#include <thread>
#include <vector>

struct Widget {
    std::string name;
    std::vector<int> data;
    explicit Widget(int i) : name("widget-" + std::to_string(i)), data(i % 64 + 1, i) {}
};

// Leaked: Widgets pushed here are never deleted.
static std::vector<Widget *> *g_leaky_cache = new std::vector<Widget *>();

static void process(int i) {
    Widget on_stack(i);            // its string/vector storage is freed at scope end
    Widget *heap = new Widget(i * 2);
    if (i % 10 == 0) {
        g_leaky_cache->push_back(new Widget(i)); // leaked into the cache
    }
    delete heap; // balanced
    (void)on_stack;
}

int main() {
    for (int i = 0;; i++) {
        process(i);
        std::this_thread::sleep_for(std::chrono::microseconds(500));
    }
}
