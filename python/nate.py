def foo(i: int) -> None:
    """this is a function i want hover on"""
    print(i)

def bar() -> None:
    return foo(4)

if __name__ == "__main__":
    bar()
