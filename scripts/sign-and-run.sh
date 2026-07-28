#!/bin/bash
# cargo run / cargo test 的 runner:先用稳定的自签名证书签名,再执行(参数原样透传)。
# 为什么需要:ad-hoc 签名的身份是二进制哈希,每次编译都变,
# macOS 钥匙串 ACL 认不出"新"二进制 → 每次访问都弹授权框。
# 用固定证书签名后,点一次"始终允许"即可(钥匙串 → 证书助理 →
# 创建证书,名称 "RShell Dev",类型"代码签名")。
set -e
bin="$1"
shift
if security find-certificate -c "RShell Dev" >/dev/null 2>&1; then
    codesign -f -s "RShell Dev" "$bin"
else
    echo "sign-and-run: 未找到 'RShell Dev' 代码签名证书,跳过签名(钥匙串仍会弹窗)" >&2
fi
exec "$bin" "$@"
