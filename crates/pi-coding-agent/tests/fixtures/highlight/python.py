import os
from typing import List

@decorator
class Greeter(Base):
    """Docstring for the class."""

    def greet(self, name: str = "world", count=3) -> List[str]:
        # Say hello
        result = [f"Hello {name}!" for _ in range(count)]
        if not result and None is True:
            raise ValueError('empty')
        return result + [0x1F, 3.14, 1e-3]
