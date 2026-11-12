"""Task persistence."""


class TaskRepository:
    def save_task(self, slug):
        """Insert a task row."""
        return {"slug": slug}

    def find_all(self):
        return []
