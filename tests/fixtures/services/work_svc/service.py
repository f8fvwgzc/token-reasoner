"""Task business logic."""
from .repository import TaskRepository
from shared import util


class TaskService:
    def create_task(self, title):
        """Validate and store a new task."""
        self.validate_title(title)
        slug = util.slugify(title)
        return TaskRepository().save_task(slug)

    def list_tasks(self):
        return TaskRepository().find_all()

    def validate_title(self, title):
        if not title:
            raise ValueError("empty title")
