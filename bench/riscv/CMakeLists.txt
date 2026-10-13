# libriscv harness (rv_bench.cpp) and the native baseline (guest.c built for
# the host). LIBRISCV_DIR points at a libriscv checkout; PAGED builds the
# virtual-paging variant, which supports copy-on-write forks.
cmake_minimum_required(VERSION 3.14)
project(rvbench LANGUAGES C CXX)
set(CMAKE_CXX_STANDARD 20)
option(PAGED "Virtual paging and forks instead of the flat arena" OFF)
if (PAGED)
    set(RISCV_FLAT_RW_ARENA OFF CACHE BOOL "" FORCE)
endif()
# Binary translation through the system compiler (gcc -O2), which the
# libriscv docs say is faster than the embedded TinyCC.
set(RISCV_LIBTCC OFF CACHE BOOL "" FORCE)
add_subdirectory(${LIBRISCV_DIR}/lib libriscv)

add_library(native_guest STATIC guest.c)
target_compile_definitions(native_guest PRIVATE NATIVE)
target_compile_options(native_guest PRIVATE -O2)

add_executable(rv-bench rv_bench.cpp)
target_link_libraries(rv-bench riscv native_guest)
if (PAGED)
    target_compile_definitions(rv-bench PRIVATE PAGED)
endif()
