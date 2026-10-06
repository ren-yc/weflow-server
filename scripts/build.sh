#!/usr/bin/env bash
# Build wrapper for Linux/macOS: system perl + C toolchain (gcc/clang/make)
# are used directly by cc (libsqlite3-sys) and perl+make (openssl-src).
# No vcvars equivalent exists on these platforms.
# Usage: bash scripts/build.sh [cargo args...]
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v perl >/dev/null 2>&1; then
  echo "warning: perl not found (openssl-src needs it for Configure)." >&2
  echo "         Linux: apt install perl | macOS: ships with the system." >&2
fi
if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
  echo "warning: no C compiler found (libsqlite3-sys needs one)." >&2
  echo "         Linux: apt install build-essential | macOS: xcode-select --install" >&2
fi
if [[ "$(uname -s)" == "Darwin" ]] && ! command -v make >/dev/null 2>&1; then
  echo "warning: make not found (openssl-src needs it). macOS: xcode-select --install" >&2
fi

# `testing` feature 只影响**可见性**：默认构建下实现面是 pub(crate)（边界由编译器强制），
# 开了它才转成 pub —— 而集成测试在独立 crate 里，只能看见 pub。所以凡是编译测试的命令都要
# 带上它，否则报的是「模块是私有的」，与真正的问题无关。
#
# 在这里补而不是写进文档：忘记它得到的是一堆看不懂的隐私错误，而不是一个明确的提示。
# 已显式给过 feature 相关参数时不插手（尊重调用方的选择）。
# 判定只看 `--` 之前的那段：`--` 之后是转发给测试二进制／rustc 的参数，那里出现同名词不代表
# 调用方给 cargo 指定过 feature（当成指定过就会漏注入，报错是一堆「模块是私有的」）。
cargo_side=(); rest_side=(); seen_dash=0
for a in "$@"; do
  if [[ $seen_dash -eq 0 && "$a" != "--" ]]; then cargo_side+=("$a"); continue; fi
  seen_dash=1; rest_side+=("$a")
done
# 子命令不假定在 "$1"：--locked／--config 是 cargo 的全局选项；这些选项的值紧跟其后，
# 跳过值才轮得到子命令。
value_opts=' -p --package --config -Z --target --target-dir --manifest-path --color --message-format --profile '
sub=''; sub_idx=-1; skip=0
for ((i = 0; i < ${#cargo_side[@]}; i++)); do
  t="${cargo_side[$i]}"
  if [[ $skip -eq 1 ]]; then skip=0; continue; fi
  if [[ "${t:0:1}" == "-" ]]; then
    if [[ "$value_opts" == *" $t "* ]]; then skip=1; fi
    continue
  fi
  sub="$t"; sub_idx=$i; break
done
# 调用方自己指定过 feature 就不插手；--features=x 与 -Fxyz 的合并形态也算指定过
# （早先只比 --features，这两种写法会被当成没指定，于是重复注入）。
already_has=0; selects_package=0
for t in ${cargo_side[@]+"${cargo_side[@]}"}; do
  case "$t" in
    --all-features|--no-default-features|--features|-F) already_has=1 ;;
    --features=*|-F?*) already_has=1 ;;
    -p|--package|--package=*) selects_package=1 ;;
    -p?*) selects_package=1 ;;
  esac
done
# -p <crate> 选中的是另一个 package：testing 只存在于根 package，注入了 cargo 会直接报错。
needs_testing=0
if [[ "$sub" == "test" || ( "$sub" == "clippy" && " ${cargo_side[*]-} " == *" --all-targets "* ) ]]; then
  needs_testing=1
fi
if [[ $needs_testing -eq 1 && $already_has -eq 0 && $selects_package -eq 0 && $sub_idx -ge 0 ]]; then
  # 引号不可省：无引号的数组展开会把含空格的元素重新分词、把带 glob 字符的
  # 路径就地展开（`--config "a b"`、`--target-dir t*`），参数在注入这一步就被改坏。
  head=("${cargo_side[@]:0:$sub_idx}")   # 全局选项在前，注入点紧跟子命令
  tail=("${cargo_side[@]:$((sub_idx + 1))}")
  set -- ${head[@]+"${head[@]}"} "$sub" --features testing ${tail[@]+"${tail[@]}"} ${rest_side[@]+"${rest_side[@]}"}
  echo "build.sh: 已补 --features testing（集成测试需要它才看得见实现面）" >&2
fi
exec cargo "$@"
