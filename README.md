# carplayd


dtc -@ -I dts -O dtb -o /boot/firmware/overlays/carplay-role.dtbo carplay-role.dts

dtoverlay=dwc2,dr_mode=otg
dtoverlay=carplay-role
