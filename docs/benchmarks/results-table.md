| Backend | Create p50 (first bucket) | Create p50 (last bucket) | Kill p50 | List p50 (600 sessions) | Capture p50 | Keystroke echo p50 | Cold start p50 | Server RSS per session |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| tymux (gRPC API) | 0.44 ms | 0.53 ms | 0.10 ms | 0.89 ms | 0.34 ms | 0.03 ms | 2.70 ms | 146 KiB |
| tymux (CLI) | 1.33 ms | 1.65 ms | 1.15 ms | 2.59 ms | 2.43 ms | – | – | – |
| tmux (CLI) | 1.92 ms | 3.89 ms | 1.98 ms | 6.11 ms | 2.64 ms | – | 5.67 ms | 14 KiB |
| tmux (control mode) | 0.60 ms | 1.72 ms | 0.31 ms | 4.11 ms | 0.33 ms | 0.69 ms | 7.52 ms | 14 KiB |
