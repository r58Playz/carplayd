#!/bin/bash
ffmpeg -y -i waiting.png -f rawvideo -pix_fmt yuv420p waiting.yuv
