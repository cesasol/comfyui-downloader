# ComfyUI Desktop Integration
An utility to download, catalog and update models from civitai into a file structure comprensible for ComfyUI

## Features:

- Only to be used on GNU/Linux with SystemD
- Works as a user daemon
- Handles download queue
- Checks for updates on every model on civitai
- Verifies downloads checksum
- Handle retries on rate limit and on network failiures
- Checks for disk space before downloading
- Uses libnotify for desktop notifications
- Has an ipc interface for communication with the daemon
- cli utility to see status and add to the queue


## Future features
- Integrate with zfs for snapshot generation
- Show status as a notification on comfyui execution
- Handle comfyui as a systemd subdaemon
- Can execute saved templates with patching to update the prompt or other parameters

