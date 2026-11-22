// Only built by the opt-in Rust unit test.
final class InspectCleanup {
    public static void main(String[] args) throws Exception {
        if (args.length != 0) throw new AssertionError();
        Inspect.report(true);
    }
}
