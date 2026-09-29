#!/bin/sh
# Kitty 图形协议的参考实现，抄自规范里的 POSIX sh 示例：
# https://sw.kovidgoyal.net/kitty/graphics-protocol/
#
# 用途：和 mcat 交叉验证。如果这个能显示图片而 mcat 不能，那是 mcat 的 bug。
# 用法: sh examples/send-png.sh examples/gradient.png

img="$1"
if [ -z "$img" ]; then
    echo "用法: $0 <图片.png>" >&2
    exit 1
fi

# 单块：整段 base64 一次发出，标成最后一块（m=0）
send_one_chunk() {
    printf '\033_Ga=T,f=100,m=0;'
    base64 -i "$img"
    printf '\033\\'
}

# BSD 与 GNU 的 base64 换行参数不一样：-b 是 BSD，-w 是 GNU。
# 换出来的那一行也要标成最后一块（m=0）。
send_chunked() {
    if base64 -b 4096 -i "$img" 2>/dev/null | encode_stream; then
        return 0
    fi
    base64 -w 4096 -i "$img" 2>/dev/null | encode_stream && return 0
    openssl base64 -e -A -in "$img" | fold -b -w 4096 | encode_stream
}

# 每行一个块：第一行带完整控制数据，最后一行 m=0，中间的 m=1
encode_stream() {
    first="y"
    while IFS= read -r chunk; do
        [ -z "$chunk" ] && continue
        if [ "$first" = "y" ]; then
            printf '\033_Ga=T,f=100,m=1;%s\033\\' "$chunk"
            first="n"
        else
            printf '\033_Gm=1;%s\033\\' "$chunk"
        fi
    done
    if [ "$first" = "n" ]; then
        # 换行符让 base64 多出一个空段，末尾补一块空的收尾
        printf '\033_Gm=0;\033\\'
    fi
}

case "$(wc -c <"$img" | tr -d ' ')" in
    ''|*[!0-9]*) echo "读不到 $img" >&2; exit 1 ;;
esac

# 小于 3 KB 的图（base64 后不超过 4096）走单块路径
size=$(wc -c <"$img" | tr -d ' ')
if [ "$size" -lt 3072 ]; then
    send_one_chunk
else
    send_chunked
fi
