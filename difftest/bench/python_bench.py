def add(a, b):
    return a + b

def sum_f32(ptr, n):
    total = 0.0
    for i in range(n):
        total += read_f32(ptr + i * 4)
    return total

def hold(sec):
    import time
    time.sleep(sec)
    return "held"
