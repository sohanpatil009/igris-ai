IGRIS 20-command acceptance suite (mirrors plan.md "20-command acceptance suite").

Provide manual/automated runs and record timings to `benchmarks/results.csv`.

1. open chrome
2. switch to chrome
3. put chrome on the left half
4. mute
5. take a screenshot
6. volume up
7. lock (approval required)
8. sleep (approval required)
9. shutdown now (approval required, 30s)
10. restart now (approval required, 30s)
11. open spotify and play music
12. pause music
13. take screenshot twice
14. read clipboard
15. search for "IGRIS assistant" on youtube
16. open notes.txt
17. tell me the weather in London
18. weather in New York
19. set a timer for 30 seconds
20. list running apps

Acceptance threshold:
- >=18/20 succeed
- Sensitive actions never execute without explicit approval
- p95 per simple command <3s online, <5s offline
- no orphan child processes after runs.
