Runkis NVIDIA build
===================

A build of the VIctory compatibility layer (trustno1-coder/VIctory-NVIDIA, with renderer ports 1, 2 and 6
from avatarmanz/VIctory-7900XTX and local tuning). It runs an Xbox Series title's own executable on Windows.

NO GAME FILES ARE INCLUDED. You need your own copy of the title (Xbox Series build 1.0.2.1).

How to run
----------
1. Drag game_scarlett_master.exe onto PLAY.bat
   (or copy the game folder here and name it "game", then double-click PLAY.bat).
2. The first start is slow while shaders are translated and pipelines are built. Later starts are faster.

Needs: Windows 10/11 x64, an NVIDIA RTX card with a current driver, 32 GB of RAM (the game uses about 20 GB).

Measured: Ryzen 5 5500X3D + RTX 2070 SUPER, Series S profile at 540p (the title's own upscaler to 1080p):
about 18 fps average in the opening scene. The CPU is the limit; a faster CPU gives more.

Controls: click the picture or F9 for mouse look; WASD, mouse, Space/Backspace/X/Y = A/B/X/Y, Esc menu.

What is tuned (in scripts\play-nvidia.ps1)
------------------------------------------
- Crash diagnostics (DRED) off: 2-4x less GPU time on RTX 20 cards.
- The keep-awake GPU job off (VICTORY_CARD_AWAKE=0): +6 % fps and half the stutters on NVIDIA.
- Memory compares on the card down to 64 KB buffers (+10 %), textures re-hashed every 32nd frame.
- Unused ray-tracing passes skipped; ray tracing itself is off (much faster, some rooms darker/brighter).

Cache and saves go to "cache" and "data" next to PLAY.bat.

Licence and source
------------------
GPL-3.0-or-later (LICENSE). This repository is the complete source
(branch runkis, based on trustno1-coder/VIctory-NVIDIA).
