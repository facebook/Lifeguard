from _struct import *

__all__ = ["calcsize", "pack", "pack_into", "unpack", "unpack_from", "iter_unpack", "Struct", "error"]

def calcsize(fmt): no_effects()
def pack(fmt, *args): no_effects()
def unpack(fmt, buffer): no_effects()
def unpack_from(fmt, buffer, offset=0): no_effects()

class error(Exception): ...
