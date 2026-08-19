# Fuzz targets

Four targets, each a standard `LLVMFuzzerTestOneInput`:

| Target | Covers |
| --- | --- |
| `fuzz_terminal` | UTF-8 decoding and the ANSI/VT parser at arbitrary chunk boundaries, plus the scrollback and parser bounds |
| `fuzz_resize` | Reflow: resize storms interleaved with output floods, primary and alternate screens |
| `fuzz_protocol` | Mirror control messages and binary frame headers, including every length field |
| `fuzz_json` | The JSON parser that every control message passes through, with a serialise/re-parse identity check |

## Running them in CI

The CMake build compiles each target with `tests/fuzz/fuzz_main.cpp`, which drives a
deterministic corpus from a seeded PRNG. That runs anywhere a compiler does, needs no
special toolchain, and is what `ctest` executes:

```bash
cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug -DTM_ENABLE_ASAN=ON
cmake --build build -j
ctest --test-dir build -R fuzz
./build/tests/fuzz_terminal --iterations=200000 --seed=12345   # longer local run
./build/tests/fuzz_terminal crash-input.bin                    # replay a reproducer
```

Build with `-DTM_ENABLE_ASAN=ON` for these: the assertions in each target check the
*specified* bounds, and the sanitizer checks the ones nobody writes down.

## Running them under libFuzzer

With a clang toolchain, the same sources build as real coverage-guided fuzzers:

```bash
clang++ -std=c++17 -g -O1 -fsanitize=fuzzer,address,undefined \
  -DTM_LIBFUZZER=1 -DTM_DEBUG_BUILD=1 -Icore/include \
  tests/fuzz/fuzz_terminal.cpp $(find core/src -name '*.cpp' ! -name 'gl_renderer.cpp') \
  -lssl -lcrypto -o fuzz_terminal
./fuzz_terminal corpus/ -max_len=8192
```

`TM_LIBFUZZER` compiles out this directory's `main()` so libFuzzer can supply its own.

## What a failure means

Every target asserts specification requirements, not implementation details:

- the parser never ends in an unrecoverable state, whatever bytes arrive (§8.1);
- scrollback lines and bytes stay inside their configured limits (§8.2, §12);
- a snapshot always has exactly `rows` non-null lines and an in-range cursor (§6.2);
- offsets emitted by the protocol decoder are contiguous and non-decreasing (§7.3);
- anything the JSON parser accepts round-trips through serialisation (§7.4).

A crash or an assertion here is a specification violation, not a style problem.
