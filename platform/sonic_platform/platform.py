"""Platform entry point. xcvrd does:

    import sonic_platform.platform
    chassis = sonic_platform.platform.Platform().get_chassis()
"""
from sonic_platform_base.platform_base import PlatformBase

from .chassis import Chassis


class Platform(PlatformBase):
    def __init__(self):
        super().__init__()
        self._chassis = Chassis()

    def get_chassis(self):
        return self._chassis
