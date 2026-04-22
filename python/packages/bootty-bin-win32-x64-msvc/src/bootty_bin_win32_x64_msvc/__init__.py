from importlib.resources import files


def binary_path() -> str:
    return str(files(__package__).joinpath("bin/bootty.exe"))
