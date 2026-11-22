import java.lang.reflect.Field;
import java.net.URI;
import java.util.List;
import org.javacs.kt.Configuration;
import org.javacs.kt.KotlinLanguageServer;

/** Provision-time test, not part of the launcher, worker jar or RPC surface. */
final class ProviderLifetimeTest {
    private static Object field(Object owner, String name) throws ReflectiveOperationException {
        Field field = owner.getClass().getDeclaredField(name);
        field.setAccessible(true);
        return field.get(owner);
    }

    public static void main(String[] args) throws ReflectiveOperationException {
        var config = new Configuration();
        config.getIndexing().setEnabled(false);
        try (var server = new KotlinLanguageServer(config)) {
            var sp = server.getSourcePath();
            var uri = URI.create("file:///eon-snapshot/document0.kt");
            // Count what the pinned Kotlin compiler actually retains; a small
            // heap or a successful short session alone cannot detect this leak.
            var env = field(server.getClassPath().getCompiler(), "defaultCompileEnvironment");
            var providers = (List<?>) field(field(env, "environment"), "packagePartProviders");
            for (int i = 0; i < 32; ++i) {
                sp.put(uri, "val n = listOf(" + i + ").sum()\n", null, false);
                sp.invalidateSnapshots();
                sp.compileFiles(List.of(uri));
            }
            if (providers.size() != 1)
                throw new AssertionError("32 compilations retained " + providers.size() + " providers, expected 1");
            System.err.println("32 distinct compilations: exactly one retained package-part provider");
        }
    }
}
