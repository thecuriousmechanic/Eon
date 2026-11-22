import Amount from "./Amount.jsx";
import { quote, type Line } from "./quote";

type Props = { lines: readonly Line[] };

export function QuoteCard({ lines }: Props) {
    return <article><Amount cents={quote(lines)} /></article>;
}
