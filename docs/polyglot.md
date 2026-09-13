# Polyglot Zero-Copy Memory & Sidecar Interoperability

One of Alloy's most revolutionary capabilities is its **Zero-Copy Sidecar Architecture**.

In traditional architectures, communicating between a JavaScript web service and a Python machine learning model or C computer vision library requires:
1. Converting data structures to JSON or Protobuf.
2. Sending them across a TCP socket, Unix domain socket, or stdin pipe.
3. Parsing the serialized data in the target process.

For large datasets (e.g., a 10MB image buffer or audio spectrogram), serialization introduces tens of milliseconds of latency and doubles or triples memory usage.

---

## 1. How Zero-Copy Memory Works

Alloy eliminates serialization by leveraging operating system shared memory mappings:
- **Windows**: `CreateFileMappingW` and `MapViewOfFile`.
- **Linux & macOS**: POSIX `shm_open` or anonymous memory mapping (`mmap`).

```
+-------------------------------------------------------------+
|               Physical RAM / OS Page Cache                  |
|          [ Shared Memory Segment: Float32Array ]            |
+-------------------------------------------------------------+
               ^                               ^
               | (Raw Pointer)                 | (Raw Pointer)
               |                               |
       +-------+-------+               +-------+-------+
       |   Alloy VM    |               |  CPython VM   |
       | (JavaScript)  |               |    (NumPy)    |
       +---------------+               +---------------+
```

When Alloy allocates typed buffers (e.g. `memory.allocateFloat32Array()`), the memory is placed directly into the shared segment. When calling Python or C, Alloy passes only the **raw pointer address** and length.

**Result: Data handoff takes 0.003 ms ($O(1)$ constant time) regardless of whether the buffer is 10 KB or 10 GB.**

---

## 2. Using the Python Bridge

### JavaScript Side (`pipeline.ajs`)

```javascript
import { memory } from 'alloy:core';
import { runInference } from './model.py' as python;

// 1. Generate or read input data
const rawData = [];
for (let i = 0; i < 2048; i++) {
    rawData.push(Math.sin(i * 0.05));
}

// 2. Allocate directly in OS shared memory segment
const tensor = memory.allocateFloat32Array(rawData);

// 3. Invoke Python model directly with pointer
// No JSON stringify! No pipe overhead!
const prediction = await python.runInference(tensor.ptr, tensor.length);

print("Inference Result:", prediction);
```

### Python Side (`model.py`)

```python
import numpy as np

def runInference(ptr, length):
    """
    Directly map the C pointer from shared memory into a NumPy array
    without copying or memory allocation.
    """
    arr = np.ctypeslib.as_array(ptr, shape=(length,))
    
    # Perform vector math with BLAS acceleration
    normalized = (arr - np.mean(arr)) / np.std(arr)
    prediction = float(np.max(normalized))
    
    return {
        "score": prediction,
        "elements_processed": length
    }
```

---

## 3. Orphan Segment Cleanup (`sweepSegments()`)

Shared memory segments are backed by temporary OS files (`alloy_shm_{pid}_{id}.tmp`). When an Alloy process terminates normally, these files are deleted.

If a host process or sidecar is killed abruptly (`kill -9`), orphaned segment files could accumulate. Alloy provides automatic startup sweeps and on-demand reclamation:

```javascript
import { sweepSegments } from 'alloy:core';

// Reclaim leaked segments from previous abnormal terminations
const report = sweepSegments();
if (report.files > 0) {
    print(`Reclaimed ${report.files} orphaned segments (${report.bytes} bytes).`);
}
```
