//go:build windows

package main

import (
	"fmt"
	"os"
	"syscall"
	"unsafe"
)

// pauseIfOwnConsole keeps a console window that was opened for this process (double-click in
// Explorer) until Enter, so the output can be read. From a terminal the console is shared.
func pauseIfOwnConsole() {
	var pids [2]uint32
	n, _, _ := syscall.NewLazyDLL("kernel32.dll").NewProc("GetConsoleProcessList").Call(
		uintptr(unsafe.Pointer(&pids[0])), uintptr(len(pids)))
	if n != 1 {
		return
	}
	fmt.Fprint(os.Stderr, "\nPress Enter to close this window...")
	_, _ = stdin.ReadString('\n')
}
