MEMORY
{
  /* 512K flash total, minus the 16K reserved for the UF2 bootloader */
  FLASH (rx) : ORIGIN = 0x00004000, LENGTH = 0x0007C000
  RAM (rwx)  : ORIGIN = 0x20000000, LENGTH = 0x00030000
}
