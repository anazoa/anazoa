#!/bin/sh
set -eu

sudo apt update
sudo apt install -y nftables iputils-ping iperf3 tcpdump coturn cpio ffmpeg

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

git submodule update --init --recursive

wget https://www.archive.org/download/idiotversion2_2604_librivox/idiot_01_dostoyevsky_128kb.mp3
ffmpeg -i idiot_01_dostoyevsky_128kb.mp3 -c:a libopus -b:a 64k test/sample.opus
rm idiot_01_dostoyevsky_128kb.mp3
scripts/vad.sh test/sample.opus
