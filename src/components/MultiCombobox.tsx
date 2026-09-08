import { useId, useMemo, useState } from "react";
import { ChevronsUpDown, Search } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { cn } from "@/lib/utils";

import type { ComboboxOption } from "@/components/Combobox";

/** 可搜索、可连续勾选的下拉列表。 */
export function MultiCombobox({
  options,
  values,
  onChange,
  placeholder,
  searchPlaceholder,
  emptyText,
  selectionUnit,
  disabled,
  className,
}: {
  options: ComboboxOption[];
  values: string[];
  onChange: (values: string[]) => void;
  placeholder: string;
  searchPlaceholder: string;
  emptyText: string;
  selectionUnit: string;
  disabled?: boolean;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const id = useId();

  const selected = useMemo(() => new Set(values), [values]);
  const selectedOptions = useMemo(
    () => options.filter((option) => selected.has(option.value)),
    [options, selected],
  );
  const filtered = useMemo(() => {
    const terms = query.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
    if (terms.length === 0) return options;
    return options.filter((option) => {
      const text = `${option.label} ${option.value}`.toLocaleLowerCase();
      return terms.every((term) => text.includes(term));
    });
  }, [options, query]);

  const allFilteredSelected =
    filtered.length > 0 && filtered.every((option) => selected.has(option.value));

  const triggerText =
    selectedOptions.length === 0
      ? placeholder
      : selectedOptions.length === 1
        ? selectedOptions[0]?.label
        : `已选 ${selectedOptions.length} ${selectionUnit}`;
  const selectedTitle = selectedOptions.map((option) => option.label).join("\n");

  function toggle(value: string, checked: boolean) {
    if (checked) {
      if (!selected.has(value)) onChange([...values, value]);
      return;
    }
    onChange(values.filter((item) => item !== value));
  }

  function toggleFiltered() {
    const filteredValues = new Set(filtered.map((option) => option.value));
    if (allFilteredSelected) {
      onChange(values.filter((value) => !filteredValues.has(value)));
      return;
    }
    onChange([...new Set([...values, ...filteredValues])]);
  }

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          aria-label={`${placeholder}，${selectedOptions.length === 0 ? "尚未选择" : triggerText}`}
          disabled={disabled}
          className={cn("justify-between font-normal", className)}
        >
          <span
            title={selectedTitle || undefined}
            className={cn("truncate", selectedOptions.length === 0 && "text-muted-foreground")}
          >
            {triggerText}
          </span>
          <ChevronsUpDown className="opacity-50" aria-hidden="true" />
        </Button>
      </PopoverTrigger>

      <PopoverContent
        className="w-auto min-w-(--radix-popover-trigger-width) max-w-[min(90vw,42rem)] p-0"
        align="start"
      >
        <div className="flex items-center gap-2 rounded-t-md border-b px-3 focus-within:ring-2 focus-within:ring-ring/50 focus-within:ring-inset">
          <Search className="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
          <Input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder={searchPlaceholder}
            aria-label={searchPlaceholder.replace("…", "")}
            className="h-10 border-0 px-0 shadow-none focus-visible:ring-0"
          />
        </div>

        <div className="flex min-h-10 items-center justify-between gap-3 border-b px-3 py-1.5">
          <span className="text-xs text-muted-foreground" aria-live="polite">
            已选 {selectedOptions.length} / {options.length}
          </span>
          <div className="flex items-center gap-1">
            <Button
              type="button"
              variant="ghost"
              size="sm"
              disabled={filtered.length === 0}
              onClick={toggleFiltered}
            >
              {allFilteredSelected ? "取消当前结果" : "全选当前结果"}
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              disabled={values.length === 0}
              onClick={() => onChange([])}
            >
              清空
            </Button>
          </div>
        </div>

        <fieldset
          className="max-h-[300px] overflow-x-hidden overflow-y-auto p-1"
          aria-label={placeholder}
        >
          {filtered.length === 0 ? (
            <div className="py-6 text-center text-sm">{emptyText}</div>
          ) : (
            filtered.map((option, index) => {
              const optionId = `${id}-${index}`;
              return (
                <label
                  key={option.value}
                  htmlFor={optionId}
                  className="flex min-h-10 cursor-pointer items-center gap-2 rounded-sm px-2 py-2 text-sm hover:bg-accent focus-within:bg-accent"
                >
                  <Checkbox
                    id={optionId}
                    checked={selected.has(option.value)}
                    onCheckedChange={(checked) => toggle(option.value, checked === true)}
                  />
                  <span className="whitespace-normal">{option.label}</span>
                </label>
              );
            })
          )}
        </fieldset>
      </PopoverContent>
    </Popover>
  );
}
