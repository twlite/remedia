# Recorded benchmark medians

Each cell is baseline → changed. RSS and thread peaks are sampled lower bounds.

| Fixture / workload / pacing / sessions                       | Runs  | Wall s        | CPU s         | RSS KiB         | Threads    | Startup p99 µs | Sink delay p99 µs | Cumulative packet capacity B |
| ------------------------------------------------------------ | ----- | ------------- | ------------- | --------------- | ---------- | -------------- | ----------------- | ---------------------------- |
| generated-opus-passthrough.ogg / passthrough / unpaced / 1   | 3 / 3 | 0.001 → 0.001 | 0.019 → 0.018 | 640 → 624       | 1 → 1      | 691 → 629      | 0 → 0             | 29128 → 29128                |
| generated-opus-passthrough.ogg / passthrough / unpaced / 8   | 3 / 3 | 0.002 → 0.002 | 0.021 → 0.020 | 640 → 640       | 1 → 1      | 202 → 189      | 0 → 0             | 233024 → 233024              |
| generated-opus-passthrough.ogg / passthrough / unpaced / 32  | 3 / 3 | 0.006 → 0.005 | 0.028 → 0.024 | 14416 → 640     | 2 → 1      | 223 → 192      | 0 → 0             | 932096 → 932096              |
| generated-opus-passthrough.ogg / passthrough / unpaced / 64  | 3 / 3 | 0.011 → 0.011 | 0.033 → 0.031 | 14640 → 14560   | 2 → 3      | 218 → 203      | 0 → 0             | 1864192 → 1864192            |
| generated-opus-passthrough.ogg / passthrough / unpaced / 128 | 3 / 3 | 0.020 → 0.021 | 0.046 → 0.045 | 14288 → 14752   | 2 → 3      | 221 → 222      | 0 → 0             | 3728384 → 3728384            |
| generated-opus-passthrough.ogg / passthrough / paced / 1     | 3 / 3 | 2.001 → 2.001 | 0.037 → 0.038 | 13136 → 13104   | 2 → 2      | 590 → 574      | 276 → 264         | 29128 → 29128                |
| generated-opus-passthrough.ogg / passthrough / paced / 8     | 3 / 3 | 2.002 → 2.003 | 0.101 → 0.137 | 14496 → 14528   | 9 → 9      | 311 → 329      | 294 → 264         | 233024 → 233024              |
| generated-opus-passthrough.ogg / passthrough / paced / 32    | 3 / 3 | 2.007 → 2.009 | 0.290 → 0.530 | 18880 → 18912   | 33 → 33    | 262 → 348      | 256 → 263         | 932096 → 932096              |
| generated-opus-passthrough.ogg / passthrough / paced / 64    | 3 / 3 | 2.015 → 2.016 | 0.504 → 0.712 | 24496 → 24448   | 65 → 65    | 280 → 356      | 251 → 252         | 1864192 → 1864192            |
| generated-opus-passthrough.ogg / passthrough / paced / 128   | 3 / 3 | 2.023 → 2.024 | 1.036 → 1.125 | 35664 → 35744   | 129 → 129  | 240 → 263      | 362 → 254         | 3728384 → 3728384            |
| generated-opus-passthrough.ogg / volume / unpaced / 1        | 3 / 3 | 0.011 → 0.011 | 0.029 → 0.029 | 13616 → 13632   | 2 → 2      | 868 → 851      | 0 → 0             | 404000 → 32741               |
| generated-opus-passthrough.ogg / volume / unpaced / 8        | 3 / 3 | 0.019 → 0.017 | 0.134 → 0.123 | 17248 → 17408   | 9 → 9      | 819 → 802      | 0 → 0             | 3232000 → 261928             |
| generated-opus-passthrough.ogg / volume / unpaced / 32       | 3 / 3 | 0.061 → 0.053 | 0.482 → 0.462 | 21680 → 21952   | 12 → 11    | 6119 → 6461    | 0 → 0             | 12928000 → 1047712           |
| generated-opus-passthrough.ogg / volume / unpaced / 64       | 3 / 3 | 0.111 → 0.099 | 0.932 → 0.909 | 23888 → 24912   | 12 → 12    | 8093 → 4677    | 0 → 0             | 25856000 → 2095424           |
| generated-opus-passthrough.ogg / volume / unpaced / 128      | 3 / 3 | 0.210 → 0.201 | 1.862 → 1.811 | 30448 → 29152   | 13 → 12    | 5044 → 5333    | 0 → 0             | 51712000 → 4190848           |
| generated-opus-passthrough.ogg / volume / paced / 1          | 3 / 3 | 2.001 → 2.001 | 0.062 → 0.090 | 13648 → 13632   | 2 → 2      | 887 → 829      | 284 → 244         | 404000 → 32741               |
| generated-opus-passthrough.ogg / volume / paced / 8          | 3 / 3 | 2.004 → 2.003 | 0.232 → 0.274 | 17440 → 17632   | 9 → 9      | 711 → 551      | 313 → 271         | 3232000 → 261928             |
| generated-opus-passthrough.ogg / volume / paced / 32         | 3 / 3 | 2.010 → 2.012 | 0.616 → 0.661 | 30112 → 31024   | 33 → 33    | 567 → 631      | 251 → 249         | 12928000 → 1047712           |
| generated-opus-passthrough.ogg / volume / paced / 64         | 3 / 3 | 2.015 → 2.015 | 1.129 → 1.129 | 47024 → 48608   | 65 → 65    | 433 → 446      | 252 → 250         | 25856000 → 2095424           |
| generated-opus-passthrough.ogg / volume / paced / 128        | 3 / 3 | 2.028 → 2.029 | 2.296 → 2.294 | 80816 → 83824   | 129 → 129  | 419 → 431      | 257 → 271         | 51712000 → 4190848           |
| generated-opus-passthrough.ogg / filter / unpaced / 1        | 3 / 3 | 0.012 → 0.011 | 0.030 → 0.028 | 15040 → 14880   | 12 → 2     | 933 → 892      | 0 → 0             | 404000 → 32741               |
| generated-opus-passthrough.ogg / filter / unpaced / 8        | 3 / 3 | 0.021 → 0.021 | 0.130 → 0.136 | 22416 → 20912   | 89 → 9     | 1043 → 889     | 0 → 0             | 3232000 → 261928             |
| generated-opus-passthrough.ogg / filter / unpaced / 32       | 3 / 3 | 0.060 → 0.059 | 0.502 → 0.489 | 27840 → 27136   | 100 → 10   | 1137 → 5695    | 0 → 0             | 12928000 → 1047712           |
| generated-opus-passthrough.ogg / filter / unpaced / 64       | 3 / 3 | 0.115 → 0.124 | 0.986 → 0.983 | 30544 → 29744   | 112 → 12   | 1544 → 6244    | 0 → 0             | 25856000 → 2095424           |
| generated-opus-passthrough.ogg / filter / unpaced / 128      | 3 / 3 | 0.220 → 0.216 | 1.967 → 1.918 | 35968 → 34224   | 112 → 12   | 1763 → 6523    | 0 → 0             | 51712000 → 4190848           |
| generated-opus-passthrough.ogg / filter / paced / 1          | 3 / 3 | 2.002 → 2.002 | 0.066 → 0.085 | 15040 → 14896   | 12 → 2     | 914 → 880      | 251 → 238         | 404000 → 32741               |
| generated-opus-passthrough.ogg / filter / paced / 8          | 3 / 3 | 2.006 → 2.005 | 0.273 → 0.411 | 22432 → 21376   | 89 → 9     | 698 → 689      | 259 → 257         | 3232000 → 261928             |
| generated-opus-passthrough.ogg / filter / paced / 32         | 3 / 3 | 2.021 → 2.014 | 0.654 → 0.709 | 46864 → 42880   | 353 → 33   | 844 → 626      | 250 → 248         | 12928000 → 1047712           |
| generated-opus-passthrough.ogg / filter / paced / 64         | 3 / 3 | 2.038 → 2.022 | 1.327 → 1.152 | 79328 → 71264   | 705 → 65   | 604 → 507      | 257 → 253         | 25856000 → 2095424           |
| generated-opus-passthrough.ogg / filter / paced / 128        | 3 / 3 | 2.094 → 2.041 | 2.463 → 2.395 | 144080 → 128272 | 1409 → 129 | 596 → 497      | 254 → 252         | 51712000 → 4190848           |
| generated-tone.mp3 / passthrough / unpaced / 1               | 3 / 3 | 0.002 → 0.002 | 0.021 → 0.019 | 640 → 656       | 1 → 1      | 799 → 952      | 0 → 0             | 64000 → 5456                 |
| generated-tone.mp3 / passthrough / unpaced / 8               | 3 / 3 | 0.004 → 0.004 | 0.033 → 0.033 | 624 → 640       | 1 → 1      | 718 → 618      | 0 → 0             | 512000 → 43648               |
| generated-tone.mp3 / passthrough / unpaced / 32              | 3 / 3 | 0.009 → 0.009 | 0.086 → 0.084 | 19312 → 656     | 9 → 1      | 1199 → 1118    | 0 → 0             | 2048000 → 174592             |
| generated-tone.mp3 / passthrough / unpaced / 64              | 3 / 3 | 0.016 → 0.016 | 0.155 → 0.151 | 20384 → 20448   | 10 → 9     | 1298 → 846     | 0 → 0             | 4096000 → 349184             |
| generated-tone.mp3 / passthrough / unpaced / 128             | 3 / 3 | 0.031 → 0.030 | 0.286 → 0.286 | 20256 → 20832   | 11 → 11    | 1035 → 971     | 0 → 0             | 8192000 → 698368             |
| generated-tone.mp3 / passthrough / paced / 1                 | 3 / 3 | 0.301 → 0.301 | 0.025 → 0.025 | 13712 → 13664   | 2 → 2      | 774 → 777      | 202 → 235         | 64000 → 5456                 |
| generated-tone.mp3 / passthrough / paced / 8                 | 3 / 3 | 0.303 → 0.303 | 0.074 → 0.075 | 16896 → 17024   | 9 → 9      | 564 → 497      | 255 → 254         | 512000 → 43648               |
| generated-tone.mp3 / passthrough / paced / 32                | 3 / 3 | 0.306 → 0.307 | 0.156 → 0.154 | 28128 → 28576   | 33 → 33    | 520 → 694      | 253 → 251         | 2048000 → 174592             |
| generated-tone.mp3 / passthrough / paced / 64                | 3 / 3 | 0.308 → 0.308 | 0.209 → 0.207 | 43008 → 43696   | 65 → 65    | 394 → 509      | 241 → 247         | 4096000 → 349184             |
| generated-tone.mp3 / passthrough / paced / 128               | 3 / 3 | 0.313 → 0.314 | 0.380 → 0.381 | 70336 → 73728   | 129 → 129  | 341 → 414      | 250 → 251         | 8192000 → 698368             |
| generated-tone.mp3 / volume / unpaced / 1                    | 3 / 3 | 0.002 → 0.002 | 0.019 → 0.019 | 640 → 656       | 1 → 1      | 774 → 776      | 0 → 0             | 64000 → 5456                 |
| generated-tone.mp3 / volume / unpaced / 8                    | 3 / 3 | 0.004 → 0.004 | 0.033 → 0.034 | 624 → 656       | 1 → 1      | 677 → 529      | 0 → 0             | 512000 → 43648               |
| generated-tone.mp3 / volume / unpaced / 32                   | 3 / 3 | 0.009 → 0.009 | 0.087 → 0.085 | 19296 → 19888   | 8 → 2      | 1016 → 902     | 0 → 0             | 2048000 → 174592             |
| generated-tone.mp3 / volume / unpaced / 64                   | 3 / 3 | 0.016 → 0.017 | 0.153 → 0.151 | 20144 → 20800   | 11 → 11    | 1118 → 1011    | 0 → 0             | 4096000 → 349184             |
| generated-tone.mp3 / volume / unpaced / 128                  | 3 / 3 | 0.030 → 0.030 | 0.288 → 0.288 | 20336 → 25856   | 9 → 10     | 1093 → 966     | 0 → 0             | 8192000 → 698368             |
| generated-tone.mp3 / volume / paced / 1                      | 3 / 3 | 0.301 → 0.301 | 0.024 → 0.025 | 13696 → 13712   | 2 → 2      | 795 → 775      | 235 → 226         | 64000 → 5456                 |
| generated-tone.mp3 / volume / paced / 8                      | 3 / 3 | 0.302 → 0.303 | 0.064 → 0.072 | 16976 → 17024   | 9 → 9      | 407 → 505      | 253 → 255         | 512000 → 43648               |
| generated-tone.mp3 / volume / paced / 32                     | 3 / 3 | 0.306 → 0.305 | 0.155 → 0.133 | 28192 → 28640   | 33 → 33    | 519 → 459      | 251 → 246         | 2048000 → 174592             |
| generated-tone.mp3 / volume / paced / 64                     | 3 / 3 | 0.308 → 0.308 | 0.202 → 0.198 | 42992 → 43728   | 65 → 65    | 555 → 524      | 240 → 229         | 4096000 → 349184             |
| generated-tone.mp3 / volume / paced / 128                    | 3 / 3 | 0.313 → 0.313 | 0.380 → 0.382 | 70400 → 73712   | 129 → 129  | 341 → 450      | 250 → 250         | 8192000 → 698368             |
| generated-tone.mp3 / filter / unpaced / 1                    | 3 / 3 | 0.003 → 0.002 | 0.021 → 0.019 | 640 → 608       | 1 → 1      | 842 → 760      | 0 → 0             | 64000 → 5456                 |
| generated-tone.mp3 / filter / unpaced / 8                    | 3 / 3 | 0.006 → 0.004 | 0.039 → 0.035 | 640 → 624       | 1 → 1      | 671 → 629      | 0 → 0             | 512000 → 43648               |
| generated-tone.mp3 / filter / unpaced / 32                   | 3 / 3 | 0.017 → 0.011 | 0.097 → 0.089 | 20048 → 24176   | 44 → 9     | 817 → 789      | 0 → 0             | 2048000 → 174592             |
| generated-tone.mp3 / filter / unpaced / 64                   | 3 / 3 | 0.032 → 0.020 | 0.169 → 0.164 | 20176 → 24448   | 41 → 7     | 682 → 893      | 0 → 0             | 4096000 → 349184             |
| generated-tone.mp3 / filter / unpaced / 128                  | 3 / 3 | 0.063 → 0.038 | 0.306 → 0.300 | 27104 → 31696   | 54 → 11    | 821 → 983      | 0 → 0             | 8192000 → 698368             |
| generated-tone.mp3 / filter / paced / 1                      | 3 / 3 | 0.302 → 0.301 | 0.026 → 0.026 | 15296 → 15136   | 12 → 2     | 842 → 773      | 240 → 237         | 64000 → 5456                 |
| generated-tone.mp3 / filter / paced / 8                      | 3 / 3 | 0.307 → 0.304 | 0.083 → 0.073 | 22960 → 21792   | 89 → 9     | 735 → 541      | 256 → 254         | 512000 → 43648               |
| generated-tone.mp3 / filter / paced / 32                     | 3 / 3 | 0.317 → 0.308 | 0.165 → 0.124 | 44960 → 44048   | 353 → 33   | 647 → 398      | 247 → 271         | 2048000 → 174592             |
| generated-tone.mp3 / filter / paced / 64                     | 3 / 3 | 0.330 → 0.315 | 0.241 → 0.212 | 78176 → 72256   | 705 → 65   | 499 → 419      | 246 → 251         | 4096000 → 349184             |
| generated-tone.mp3 / filter / paced / 128                    | 3 / 3 | 0.374 → 0.326 | 0.492 → 0.401 | 133680 → 126288 | 1409 → 129 | 477 → 431      | 253 → 253         | 8192000 → 698368             |
