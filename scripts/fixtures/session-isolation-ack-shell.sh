#!/bin/sh
set -eu

ack_file=${RIO_ACCEPT_ACK_FILE:?RIO_ACCEPT_ACK_FILE is required}
count=0
printf 'READY pid=%s\n' "$$" >>"$ack_file"

while IFS= read -r line; do
    case "$line" in
        after-pointer*)
            printf 'ACK pid=%s input=after-pointer\n' "$$" >>"$ack_file"
            ;;
        after-dnd*)
            printf 'ACK pid=%s input=after-dnd\n' "$$" >>"$ack_file"
            ;;
        after-key*)
            printf 'ACK pid=%s input=after-key\n' "$$" >>"$ack_file"
            ;;
        WAYLAND_FOREIGN_DND*)
            printf 'ACK pid=%s input=WAYLAND_FOREIGN_DND\n' "$$" >>"$ack_file"
            ;;
        recovered*)
            printf 'ACK pid=%s input=recovered\n' "$$" >>"$ack_file"
            ;;
        tab-keep*)
            printf '\033[2J\033[H\033[38;5;46mSOURCE_RETAINED_TAB\033[0m\n' >&1
            printf 'ACK pid=%s input=tab-keep\n' "$$" >>"$ack_file"
            ;;
        tab-transfer*)
            printf '\033[2J\033[H\033[38;5;208mSELECTED_TRANSFERRED_TAB\033[0m\n' >&1
            printf 'ACK pid=%s input=tab-transfer\n' "$$" >>"$ack_file"
            ;;
        pane-a*)
            printf 'ACK pane-a pid=%s\n' "$$" >>"$ack_file"
            ;;
        pane-b*)
            printf 'ACK pane-b pid=%s\n' "$$" >>"$ack_file"
            ;;
        exit|quit)
            exit 0
            ;;
        ack*)
            count=$((count + 1))
            printf 'ACK pid=%s count=%s input=%s\n' "$$" "$count" "$line" >>"$ack_file"
            ;;
        a*)
            printf 'ACK pid=%s input=a\n' "$$" >>"$ack_file"
            ;;
    esac
done
