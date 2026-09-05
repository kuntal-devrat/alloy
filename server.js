import { memory, http } from 'alloy:core'

const buf = memory.allocateFloat32Array([1.5, 2.5, 3.5])
print(buf.length, buf.ptr)

const server = http.createServer(function(req, res) {
    print("handling", req.method, req.url)
    res.send({ message: "hello from alloy", answer: 40 + 2 })
})
server.listen(8080)
