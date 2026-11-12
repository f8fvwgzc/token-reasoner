"""HTTP handlers for tasks."""
from work_svc.service import TaskService
from auth_svc.tokens import verify_token


def create_task_handler(request):
    """Handle a task-creation request."""
    verify_token(request.token)
    return TaskService().create_task(request.title)


def list_tasks_handler(request):
    return TaskService().list_tasks()
