#!/usr/bin/env bash
# Static check that a binary only *unconditionally* needs x86-64 baseline (SSE2).
# Lists functions containing instructions from newer extensions (AVX*, SSE3/SSSE3/SSE4, BMI,
# POPCNT, LZCNT, AES, PCLMUL…). Such code is fine only if it is behind runtime CPU detection
# (ring, memchr, aho-corasick, rustls, miniz_oxide, …); anything else is a bug.
# usage: CARGO_PROFILE_RELEASE_STRIP=none cargo build --release -p ufrh-cli --target-dir target/baseline
#        scripts/check-baseline.sh target/baseline/release/ufrh [--list]
set -euo pipefail
bin="${1:?binary}"
# A stripped binary has no function names, so the allowlist cannot work: refuse it.
if ! nm "$bin" >/dev/null 2>&1 || [ "$(nm "$bin" 2>/dev/null | wc -l)" -lt 100 ]; then
  echo "binary is stripped; build with: CARGO_PROFILE_RELEASE_STRIP=none cargo build --release --target-dir target/baseline" >&2
  exit 2
fi
ALLOW='ring|bn_|mulx|sqrx|process_extra|nistz|fiat|vpaes|aesni|memchr|aho_corasick|regex|miniz|crc32|adler|simd|core.{1,3}arch|std.{1,3}arch|xgetbv|std_detect|chacha|poly1305|sha|aes|ghash|gcm|curve25519|p256|p384|bigint|montgomery|x86_64|avx|sse|clmul|ssse3|ssse4|bmi|rustls|hashbrown|blake|png|zune|fdeflate|tiny_skia|ttf_parser|ab_glyph|epaint|swash|fontdb|rustybuzz|memmem|jetscii|bytecount|unicode_segmentation|ufmt|base64|hex'
# tzcnt is deliberately absent: LLVM emits `rep bsf`, which objdump shows as tzcnt but old CPUs run as bsf.
OPS='^(v[a-z0-9]+|pshufb|phadd[a-z]*|phsub[a-z]*|pmaddubsw|pmulhrsw|psign[bwd]|palignr|pabs[bwd]|blendv?p[sd]|pblend[a-z]*|ptest|pmin[su][bdw]|pmax[su][bdw]|pmulld|pmuldq|pcmpeqq|pcmpgtq|pcmp[ei]str[im]|pinsr[bdq]|pextr[bdq]|pmovsx[a-z]*|pmovzx[a-z]*|packusdw|roundp[sd]|rounds[sd]|dpp[sd]|insertps|extractps|movntdqa|popcnt|lzcnt|andn|bextr|blsi|blsmsk|blsr|bzhi|pdep|pext|mulx|rorx|sarx|shlx|shrx|crc32|aes[a-z]*|pclmulqdq|sha[0-9a-z]+|adcx|adox|movbe|rdrand|rdseed|f?madd[0-9a-z]*|fmsub[0-9a-z]*|lddqu|haddp[sd]|hsubp[sd]|addsubp[sd]|movs[hl]dup|movddup|monitor|mwait)$'
objdump -d --no-show-raw-insn -M intel "$bin" 2>/dev/null | awk -v ops="$OPS" '
  /^[0-9a-f]+ <.*>:$/ { fn=$0; next }
  { n=split($0, a, "\t"); if (n>=2) { split(a[2], b, " "); m=b[1]; if (m ~ ops) { hits[fn]++; kinds[fn]=kinds[fn] " " m } } }
  END { for (f in hits) print hits[f] "\t" f "\t" kinds[f] }' | sort -rn > /tmp/ufrh-baseline-hits.txt
total=$(wc -l < /tmp/ufrh-baseline-hits.txt)
bad=$(grep -viE "$ALLOW" /tmp/ufrh-baseline-hits.txt || true)
echo "functions with post-SSE2 instructions: $total"
if [ "${2:-}" = "--list" ]; then cat /tmp/ufrh-baseline-hits.txt | cut -c1-200 | head -60; fi
if [ -n "$bad" ]; then echo "NOT in a known runtime-dispatched crate:"; echo "$bad" | cut -c1-220 | head -40; exit 1; fi
echo "OK: every such function belongs to a runtime-dispatched crate (allowlist)."
