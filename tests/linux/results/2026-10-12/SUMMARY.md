# Linux matrix 2026-10-12 (cells: before at a4528d7 -> now at 46c6074 + 4 portability fixes; "pN" passed, "fN" failed)

| crate | debian | ubuntu | fedora | arch | alpine |
|---|---|---|---|---|---|
| seedwell-bencode | p24 -> p24 | p24 -> p24 | p24 -> p24 | p24 -> p24 | p24 -> p24 |
| seedwell-metainfo | p67 -> p67 | p67 -> p67 | p67 -> p67 | p67 -> p67 | p67 -> p67 |
| seedwell-wire | p84 -> p84 | p84 -> p84 | p84 -> p84 | p84 -> p84 | p84 -> p84 |
| seedwell-tracker | p324 -> p324 | p324 -> p324 | p324 -> p324 | p324 -> p324 | p324 -> p324 |
| seedwell-storage | p74 -> p74 | p74 -> p74 | p74 -> p74 | p74 -> p74 | p74 -> p74 |
| seedwell-dht | p96 -> p96 | p96 -> p96 | p96 -> p96 | p96 -> p96 | p96 -> p96 |
| seedwell-utp | p76 -> p76 | p76 -> p76 | p76 -> p76 | p76 -> p76 | p76 -> p76 |
| seedwell-http | p54 -> p54 | p54 -> p54 | p54 -> p54 | p54 -> p54 | p54 -> p54 |
| seedwell-state | p139 -> p142 | p139 -> p142 | p139 -> p142 | p139 -> p142 | p139 -> p142 |
| seedwell-classify | p157 -> p162 | p157 -> p162 | p157 -> p162 | p157 -> p162 | p157 -> p162 |
| seedwell-engine | p247/f3 -> p258 | p246/f4 -> p258 | p249/f1 -> p258 | p246/f4 -> p257/f1 | p247/f3 -> p258 |
| seedwell-control | p86 -> p87 | p86 -> p87 | p86 -> p87 | p86 -> p87 | p86 -> p87 |
| seedwell-cli | p31 -> p31 | p31 -> p31 | p31 -> p31 | p31 -> p31 | p31 -> p31 |
| seedwell-portmap | p71 -> p71 | p71 -> p71 | p71 -> p71 | p71 -> p71 | p71 -> p71 |
| total | 1530/f3 -> 1550/f0 | 1529/f4 -> 1550/f0 | 1532/f1 -> 1550/f0 | 1529/f4 -> 1549/f1 | 1530/f3 -> 1550/f0 |

Now-run notes: arch seedwell-engine 1 failure = the_dht_works_on_the_shared_port (fixed by utp/src/driver.rs; the 5 distro runs predate it). Verify run (debian, seedwell-utp + seedwell-engine, all fixes): seedwell-engine 258 pass 0 fail; seedwell-utp one load-flake (large_transfer_is_byte_exact_default_mtu 3.5 MB/s < 5 MB/s floor while host load was 8; 290 MB/s alone).
uTP PMTU on Linux 6.8 (colima, lo MTU 65536): IP_MTU_DISCOVER and IPV6_MTU_DISCOVER read back PROBE then DONT (new test linux_pmtu_discovery_reads_back_probe_then_dont); loopback_grows_the_datagram_past_1400_by_probing passes on all 5 distros, settled at 4000 B.
