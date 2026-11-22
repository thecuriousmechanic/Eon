final class OwnedFixture {
    private static final class Marker {}
    private static final Marker[] live = new Marker[3];
    private static int visits;

    private static void mark() {
        visits++;
    }

    private static void first() {
        live[0] = new Marker();
        mark();
    }

    private static void selected() {
        live[1] = new Marker();
        live[2] = new Marker();
        mark();
    }

    public static void main(String[] args) {
        first();
        selected();
        mark();
        if (visits != 3) throw new AssertionError();
    }
}
