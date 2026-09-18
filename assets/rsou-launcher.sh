#!/bin/sh
# rsou 启动器(Debian 包安装为 /usr/bin/rsou,真正的二进制在
# /usr/lib/rsou/rsou)。
#
# 动态链接器报错(缺少硬链接库、glibc 版本不够)和 exec 失败都发生在程序自己的
# 诊断代码运行之前,程序来不及写日志;而 .desktop 的 Terminal=false 又把标准错误
# 藏了起来,表现就是"双击了但什么都没发生"。因此由脚本先把 stderr 重定向进日志
# 文件,再启动程序:这样链接器给出的原因和程序自身的诊断会落在同一次启动的记录里。
#
# 脚本负责日志轮转,并把 RSOU_LAUNCHER 传给程序;程序据此改为追加写入,
# 不会把刚写进去的链接器报错截断。直接运行裸二进制时,轮转由程序自己做。

set -u

program_name=rsou

# 允许用环境变量直接指定二进制,便于在未安装的情况下验证启动器行为。
real_binary=${RSOU_BIN:-}

if [ -z "$real_binary" ]; then
    # 通过 PATH 调用时 $0 只有文件名,先还原成实际路径再取所在目录。
    case $0 in
        */*) self_path=$0 ;;
        *) self_path=$(command -v -- "$0" 2>/dev/null) || self_path=$0 ;;
    esac
    self_dir=$(cd -- "$(dirname -- "$self_path")" 2>/dev/null && pwd -P) \
        || self_dir=$(dirname -- "$self_path")
    real_binary=$self_dir/../lib/$program_name/$program_name
fi

# 日志位置必须与 src/startup.rs 里的候选顺序保持一致,否则两边会写到不同文件。
if [ -n "${XDG_STATE_HOME:-}" ]; then
    log_file=$XDG_STATE_HOME/$program_name/startup.log
elif [ -n "${HOME:-}" ]; then
    log_file=$HOME/.cache/$program_name/startup.log
else
    log_file=/tmp/rsou-startup.log
fi

log_dir=$(dirname -- "$log_file")
if ! { mkdir -p -- "$log_dir" 2>/dev/null && [ -w "$log_dir" ]; }; then
    # 日志目录不可写时放弃重定向,而不是让重定向失败导致程序根本起不来。
    log_file=
fi

if [ -n "$log_file" ]; then
    # 保留上一次启动的日志:偶发故障往往伴随一次成功启动,不轮转就会丢掉失败证据。
    if [ -s "$log_file" ]; then
        mv -f -- "$log_file" "$log_file.1" 2>/dev/null || log_file=
    fi
    # 轮转后再确认能追加;确认不了就退回不重定向。
    if [ -n "$log_file" ] && ! : >>"$log_file" 2>/dev/null; then
        log_file=
    fi
fi

if [ ! -x "$real_binary" ]; then
    message="$program_name: 找不到可执行文件 $real_binary,安装可能不完整。"
    printf '%s\n' "$message" >&2
    if [ -n "$log_file" ]; then
        printf '%s\n' "$message" >>"$log_file" 2>/dev/null
    fi
    exit 127
fi

if [ -n "$log_file" ]; then
    # 只有确实接管了日志才通知程序改为追加;否则让程序按自己的规则轮转,
    # 免得它以为脚本已经轮转过、结果日志变成无限追加。
    export RSOU_LAUNCHER=1
    if [ -t 2 ] && command -v tee >/dev/null 2>&1; then
        # 从终端启动时保留屏幕上的错误输出,同时留一份到日志(交互场景下的退出码
        # 取自 tee;桌面启动走下面的 exec,退出码原样传递)。
        "$real_binary" "$@" 2>&1 | tee -a -- "$log_file" 1>&2
        exit $?
    fi
    # 桌面启动:stderr 只进日志。链接器在 main() 之前报的错因此也留得下来。
    # 告诉程序 stderr 已经进日志,免得它把自己写的失败原因在同一份日志里打第二遍。
    export RSOU_STDERR_IN_LOG=1
    exec "$real_binary" "$@" 2>>"$log_file"
fi

exec "$real_binary" "$@"
