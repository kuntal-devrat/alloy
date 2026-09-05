// t12: stateful closures — bank account, queue, stack, toggle — objects of
// arrow functions sharing upvalues.
function createBank() {
    let balance = 0;
    return {
        deposit: (amount) => { balance += amount; return balance; },
        withdraw: (amount) => {
            if (amount > balance) { throw "insufficient"; }
            balance -= amount;
            return balance;
        },
        getBalance: () => balance
    };
}
let bank = createBank();
bank.deposit(100);
bank.deposit(50);
bank.withdraw(30);
print("bank", bank.getBalance());
let err = "";
try { bank.withdraw(500); } catch (e) { err = e; }
print("bank-err", err);
function createQueue() {
    let items = [];
    return {
        enqueue: (x) => { items[items.length] = x; },
        dequeue: () => {
            if (items.length === 0) { return undefined; }
            let v = items[0];
            let next = [];
            for (let i = 1; i < items.length; i++) { next[i - 1] = items[i]; }
            items = next;
            return v;
        },
        size: () => items.length
    };
}
let q = createQueue();
q.enqueue(1);
q.enqueue(2);
q.enqueue(3);
print("queue", q.dequeue(), q.dequeue(), q.size(), q.dequeue());
print("queue-empty", q.dequeue(), q.size());
function createStack() {
    let items = [];
    return {
        push: (x) => { items[items.length] = x; },
        pop: () => {
            if (items.length === 0) { return undefined; }
            let v = items[items.length - 1];
            let next = [];
            for (let i = 0; i < items.length - 1; i++) { next[i] = items[i]; }
            items = next;
            return v;
        },
        size: () => items.length
    };
}
let st = createStack();
st.push("a");
st.push("b");
st.push("c");
print("stack", st.pop(), st.pop(), st.size(), st.pop(), st.size());
function makeToggle() {
    let on = false;
    return {
        flip: () => { on = !on; return on; },
        state: () => on
    };
}
let tgl = makeToggle();
print("toggle", tgl.state(), tgl.flip(), tgl.flip(), tgl.flip());
function makeAccumulator() {
    let acc = 0;
    return (x) => { acc += x; return acc; };
}
let accFn = makeAccumulator();
print("accum", accFn(1), accFn(2), accFn(3));
function counterFactory(start, step) {
    let cur = start;
    return {
        next: () => { cur += step; return cur; },
        reset: () => { cur = start; return cur; }
    };
}
let cf = counterFactory(0, 5);
print("counter", cf.next(), cf.next(), cf.reset(), cf.next());
