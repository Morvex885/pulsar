//go:build !windows

package main

// pauseIfOwnConsole: only Windows opens a console window that closes with the process.
func pauseIfOwnConsole() {}
