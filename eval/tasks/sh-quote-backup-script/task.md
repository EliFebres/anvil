`scripts/backup.sh` breaks on file names that contain spaces. Fix the quoting so every file under `data/`, whatever its name, is copied to `out/` under the same name. Keep the final summary line.
