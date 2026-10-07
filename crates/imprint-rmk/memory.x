MEMORY
{
  /* Go60 uses the same MoErgo nRF52840 bootloader/storage boundaries as the
   * Glove80. The application is capped at 0xdc000; 0xdc000-0xf4000 is the
   * RMK settings store (keyboard.toml `[storage]`) and 0xf4000+ the
   * bootloader, so this image cannot collide with either.
   */
  FLASH : ORIGIN = 0x00026000, LENGTH = 0xB6000
  /* The last 256 bytes of app RAM (0x2003FB08..0x2003FC08) are the panic
   * store: a fixed cross-build address, so a stable image can read the
   * report a crashing image persisted. Owned by neither the stack (which
   * starts at the shrunken RAM top) nor the bootloader (above 0x2003FC08).
   */
  RAM : ORIGIN = 0x20000008, LENGTH = 255K - 256
}
