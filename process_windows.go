package main

import (
	"os/exec"
	"strconv"
	"time"
)

func configureCommand(cmd *exec.Cmd) {
	cmd.Cancel = func() error {
		// Kill the tree as well as the interpreter running optional codec processes.
		kill := exec.Command("taskkill", "/F", "/T", "/PID", strconv.Itoa(cmd.Process.Pid))
		if err := kill.Run(); err != nil {
			return cmd.Process.Kill()
		}
		return nil
	}
	cmd.WaitDelay = 5 * time.Second
}
