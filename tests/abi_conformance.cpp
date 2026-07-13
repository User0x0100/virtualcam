#include <cstddef>
#include <cstdint>
#include <windows.h>

struct QueueHeader {
    volatile std::uint32_t write_idx;
    volatile std::uint32_t read_idx;
    volatile std::uint32_t state;
    std::uint32_t offsets[3];
    std::uint32_t type;
    std::uint32_t cx;
    std::uint32_t cy;
    std::uint64_t interval;
    std::uint32_t reserved[8];
};

struct SharedMemHeader {
    DWORD maxSize;
    int width;
    int height;
    int stride;
    int format;
    int resizemode;
    int mirrormode;
    int timeout;
    std::uint8_t data[1];
};

static_assert(sizeof(std::uint32_t) == 4);
static_assert(sizeof(std::uint64_t) == 8);
static_assert(sizeof(DWORD) == 4);
static_assert(sizeof(int) == 4);
static_assert(sizeof(long) == 4);

static_assert(sizeof(QueueHeader) == 80);
static_assert(alignof(QueueHeader) == 8);
static_assert(offsetof(QueueHeader, write_idx) == 0);
static_assert(offsetof(QueueHeader, read_idx) == 4);
static_assert(offsetof(QueueHeader, state) == 8);
static_assert(offsetof(QueueHeader, offsets) == 12);
static_assert(offsetof(QueueHeader, type) == 24);
static_assert(offsetof(QueueHeader, cx) == 28);
static_assert(offsetof(QueueHeader, cy) == 32);
static_assert(offsetof(QueueHeader, interval) == 40);
static_assert(offsetof(QueueHeader, reserved) == 48);

static_assert(sizeof(SharedMemHeader) == 36);
static_assert(alignof(SharedMemHeader) == 4);
static_assert(offsetof(SharedMemHeader, maxSize) == 0);
static_assert(offsetof(SharedMemHeader, width) == 4);
static_assert(offsetof(SharedMemHeader, height) == 8);
static_assert(offsetof(SharedMemHeader, stride) == 12);
static_assert(offsetof(SharedMemHeader, format) == 16);
static_assert(offsetof(SharedMemHeader, resizemode) == 20);
static_assert(offsetof(SharedMemHeader, mirrormode) == 24);
static_assert(offsetof(SharedMemHeader, timeout) == 28);
static_assert(offsetof(SharedMemHeader, data) == 32);

int main() { return 0; }
