#!/bin/sh
set -eu

ack_file=${RIO_ACCEPT_ACK_FILE:?RIO_ACCEPT_ACK_FILE is required}
hold=${RIO_ACCEPT_FIXTURE_HOLD:-1}
kitty_payload='/wAA/wD/AP8AAP///////w=='

printf 'READY pid=%s\n' "$$" >"$ack_file"

if [ "${RIO_ACCEPT_PROFILE:-0}" = 1 ]; then
    printf 'PROFILE_START\n' >>"$ack_file"
    frame=0
    while [ "$frame" -lt 600 ]; do
        printf '\033[2J\033[H\033[38;5;45mGPU profile frame %03d\033[0m\n' "$frame"
        printf '\033[31mred\033[0m  \033[1;34mbold-blue\033[0m  \033[4munderlined\033[0m\n'
        printf '\033[38;5;214mxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\033[0m\n'
        printf '\033[38;5;82myyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy\033[0m\n'
        frame=$((frame + 1))
        sleep 0.01
    done
    printf 'PROFILE_FRAMES=%s\n' "$frame" >>"$ack_file"
    while :; do
        sleep 60
    done
fi

draw_main() {
    printf '\033]4;1;rgb:20/40/c0\007'
    printf '\033[?1049l\033[2J\033[H'
    printf '\033[38;5;45mGPU visual fixture\033[0m\n'
    printf '\033[31mred\033[0m  \033[1;34mbold-blue\033[0m  \033[4munderlined\033[0m\n'
    printf '\033]8;;https://example.invalid\007OSC-8 link\033]8;;\007\n'
    printf '\033[38;5;214mpalette and cursor probe\033[0m\n'
    printf 'graphics below; the checkerboard is deterministic\n'
    printf '\033[?25h\033[6 q'
}

draw_underline() {
    draw_main
    printf '\033[10;1H\033[4:2mDOUBLE UNDERLINE\033[4:0m\n'
    printf '\033[11;1H\033[4:3mCURLY UNDERLINE\033[4:0m\n'
    printf '\033[12;1H\033[4:4mDOTTED UNDERLINE\033[4:0m\n'
    printf '\033[13;1H\033[4:5mDASHED UNDERLINE\033[4:0m\n'
    printf '\033[14;1H\033[58:2::255:128:0mCOLORED UNDERLINE\033[59m\n'
}

draw_kitty() {
    draw_main
    printf '\033[7;4H'
    printf '\033_Gf=32,s=2,v=2,i=7,a=T,x=0,y=0,w=2,h=2,c=8,r=4;%s\033\\' "$kitty_payload"
    printf '\033[17;1Hkitty RGBA image\n'
}

draw_sixel() {
    draw_main
    printf '\033[7;4H'
    printf '\033P0;0;0q#0;2;100;0;0#1;2;0;0;100#0!48~-#1!48~-\033\\'
    printf '\033[17;1Hsixel red-blue bands\n'
}

draw_alternate() {
    printf '\033[?1049h\033[2J\033[H'
    printf '\033[38;5;82malternate screen\033[0m\n'
    printf '\033[1;35mbold alternate\033[0m  \033[4munderlined alternate\033[0m\n'
    printf '\033]8;;https://example.invalid\007alternate OSC-8\033]8;;\007\n'
    printf '\033[5;1Hcursor mode: block\n'
    printf '\033[?25h\033[2 q'
}

state() {
    local name=$1
    case "$name" in
        base|palette|cursor) draw_main ;;
        underline) draw_underline ;;
        kitty) draw_kitty ;;
        sixel) draw_sixel ;;
        alternate) draw_alternate ;;
        *) printf 'unknown fixture state: %s\n' "$name" >&2; exit 2 ;;
    esac
    case "$name" in
        palette)
            printf '\033]4;1;rgb:ff/00/00\007'
            printf '\033[10;1H\033[38;5;1mOSC-4 palette mutated\033[0m\n'
            ;;
        cursor) printf '\033[?25l\033[11;1Hhidden cursor\033[?25h\033[2 q\n' ;;
    esac
    printf 'STATE=%s\n' "$name" >>"$ack_file"
    sleep "$hold"
}

for name in base palette cursor underline kitty sixel alternate; do
    state "$name"
done

printf 'DONE\n' >>"$ack_file"

while :; do
    sleep 60
done
